"""Advisory, non-executing Python test-oracle analysis.

Adapted from the local KDA source-only analyzer. Findings are review prompts,
not proof that an assertion is unnecessary or that unflagged tests are adequate.

Input: {"sources": {"relative/path.py": "source"}}. Never import analyzed code.
Unknown dynamic calls are retained as coverage limitations, not behavior proof.
"""

import ast
import hashlib
import json
import sys


MAX_INPUT_BYTES = 16 * 1024 * 1024
MAX_SOURCE_FILES = 10000


def spelling(node):
    if isinstance(node, ast.Name):
        return node.id
    if isinstance(node, ast.Attribute):
        base = spelling(node.value)
        return base + "." + node.attr if base else ""
    return ""


def normalized(node):
    return ast.dump(node, include_attributes=False)


def descendants(node):
    """Do not credit deferred nested functions/lambdas as executed."""
    yield node
    for child in ast.iter_child_nodes(node):
        if isinstance(
            child, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef, ast.Lambda)
        ):
            continue
        yield from descendants(child)


def bindings(function):
    result = {}
    for node in descendants(function):
        if isinstance(node, ast.Assign):
            for target in node.targets:
                if isinstance(target, ast.Name):
                    result[target.id] = node.value if target.id not in result else None
        elif isinstance(node, ast.AnnAssign) and isinstance(node.target, ast.Name):
            result[node.target.id] = (
                node.value if node.target.id not in result else None
            )
    return result


def expand(node, env, seen=frozenset()):
    if isinstance(node, ast.Name) and node.id in env and node.id not in seen:
        value = env[node.id]
        if value is not None:
            return expand(value, env, seen | {node.id})
    return node


def value_inputs(node, env, seen=frozenset()):
    """Transitive inputs of pure expressions; None means an unresolved effect.

    Absence of a parameter is proof only for this closed expression subset.
    Calls, attributes and mutation can hide dependencies and stay unknown.
    """
    if isinstance(node, ast.Name):
        if node.id in env:
            if node.id in seen or env[node.id] is None:
                return None
            return value_inputs(env[node.id], env, seen | {node.id})
        return {node.id}
    if isinstance(
        node,
        (
            ast.Call,
            ast.Attribute,
            ast.Subscript,
            ast.NamedExpr,
            ast.comprehension,
            ast.Lambda,
        ),
    ):
        return None
    result = set()
    for child in ast.iter_child_nodes(node):
        inputs = value_inputs(child, env, seen)
        if inputs is None:
            return None
        result.update(inputs)
    return result


def traced_nodes(node, env, seen=frozenset()):
    """Read-only expression provenance, including immutable local aliases."""
    yield node
    if isinstance(node, ast.Name) and node.id in env and node.id not in seen:
        if env[node.id] is not None:
            yield from traced_nodes(env[node.id], env, seen | {node.id})
    else:
        for child in ast.iter_child_nodes(node):
            yield from traced_nodes(child, env, seen)


def repository_read(node):
    if not isinstance(node, ast.Call):
        return False
    method = (
        node.func.attr if isinstance(node.func, ast.Attribute) else spelling(node.func)
    )
    if method not in {"read_text", "read_bytes", "open", "read"}:
        return False
    return any(
        isinstance(n, ast.Constant)
        and isinstance(n.value, str)
        and n.value.lower().endswith(
            (".rs", ".py", ".toml", ".json", ".yaml", ".yml", ".ps1", "justfile")
        )
        for n in ast.walk(node)
    )


def expression_key(node, env, seen=frozenset()):
    """AST identity with local aliases substituted, without changing literals."""
    if (
        isinstance(node, ast.Name)
        and node.id in env
        and node.id not in seen
        and env[node.id] is not None
    ):
        return expression_key(env[node.id], env, seen | {node.id})
    if isinstance(node, ast.AST):
        return (
            type(node).__name__,
            tuple(
                (field, expression_key(value, env, seen))
                for field, value in ast.iter_fields(node)
                if field != "ctx"
            ),
        )
    if isinstance(node, list):
        return tuple(expression_key(value, env, seen) for value in node)
    return node


class Program:
    def __init__(self, sources):
        self.sources = sources
        self.modules = {}
        self.functions = {}
        self.constants = {}
        self.aliases = {}
        self.unknown = []
        for path, source in sorted(sources.items()):
            module = path.replace("\\", "/").removesuffix(".py").replace("/", ".")
            if module.endswith(".__init__"):
                module = module[:-9]
            try:
                tree = ast.parse(source, filename=path)
            except (SyntaxError, ValueError, RecursionError) as error:
                self.unknown.append(
                    {"path": path, "reason": "parse_error", "detail": str(error)}
                )
                continue
            self.modules[module] = (path, tree)
            aliases = {}
            for node in tree.body:
                if isinstance(node, ast.Import):
                    for alias in node.names:
                        aliases[alias.asname or alias.name.split(".")[0]] = (
                            alias.name if alias.asname else alias.name.split(".")[0]
                        )
                elif isinstance(node, ast.ImportFrom):
                    prefix = node.module or ""
                    if node.level:
                        parts = module.split(".")[: -node.level]
                        prefix = ".".join([*parts, prefix]).strip(".")
                    for alias in node.names:
                        aliases[alias.asname or alias.name] = prefix + "." + alias.name
                elif isinstance(node, ast.Assign) and isinstance(
                    node.value, ast.Constant
                ):
                    for target in node.targets:
                        if isinstance(target, ast.Name):
                            self.constants[module + "." + target.id] = node.value
            self.aliases[module] = aliases
            self.collect(module, path, tree.body, "")

    def collect(self, module, path, nodes, owner):
        for node in nodes:
            if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                identity = ".".join(filter(None, [module, owner, node.name]))
                self.functions[identity] = (
                    module,
                    path,
                    node,
                    node.name.startswith("test"),
                )
            elif isinstance(node, ast.ClassDef):
                self.collect(
                    module, path, node.body, ".".join(filter(None, [owner, node.name]))
                )

    def resolve(self, module, node):
        name = spelling(node)
        head, dot, tail = name.partition(".")
        if head in self.aliases[module]:
            return self.aliases[module][head] + (dot + tail if dot else "")
        candidate = module + "." + name
        return (
            candidate
            if candidate in self.functions or candidate in self.constants
            else name
        )

    def reached(self, identity):
        reached = set()
        queue = [identity]
        while queue:
            current = queue.pop()
            module, _, function, _ = self.functions[current]
            for node in descendants(function):
                if not isinstance(node, ast.Call):
                    continue
                callee = self.resolve(module, node.func)
                if (
                    callee in self.functions
                    and callee != identity
                    and callee not in reached
                ):
                    reached.add(callee)
                    queue.append(callee)
        return reached

    def returns(self, identity):
        """Closed, authored return expressions only; never execute a helper."""
        function = self.functions[identity][2]
        env = bindings(function)
        returns = [
            expand(n.value, env)
            for n in descendants(function)
            if isinstance(n, ast.Return) and n.value
        ]
        return returns, env

    def output_fields(self, identity):
        returns, env = self.returns(identity)
        if not returns or any(not isinstance(value, ast.Dict) for value in returns):
            return None
        # Mutation/escape can introduce keys or change values; closed dictionaries
        # require direct literal returns, not a mutable local returned later.
        function = self.functions[identity][2]
        if any(
            isinstance(n, ast.Return) and not isinstance(n.value, ast.Dict)
            for n in descendants(function)
        ):
            return None
        fields = {}
        for value in returns:
            for key, expression in zip(value.keys, value.values):
                if not isinstance(key, ast.Constant) or not isinstance(key.value, str):
                    return None
                fields.setdefault(key.value, []).append(expand(expression, env))
        return fields


def assertions(function):
    for node in descendants(function):
        if isinstance(node, ast.Assert):
            yield node, "assert", [node.test]
        elif isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute):
            name = node.func.attr
            if name.startswith("assert"):
                yield node, name, node.args


def effects(program, identity):
    module, _, function, _ = program.functions[identity]
    result = []
    for node in descendants(function):
        if isinstance(node, ast.Call):
            target = program.resolve(module, node.func)
            if target in {
                "subprocess.run",
                "subprocess.Popen",
                "subprocess.call",
                "subprocess.check_call",
                "subprocess.check_output",
                "builtins.open",
                "open",
            }:
                result.append((target, node))
    return result


def patch_targets(program, module, function):
    return set(patch_bindings(program, module, function).values())


def patch_bindings(program, module, function):
    def target(node):
        if not isinstance(node, ast.Call):
            return None
        name = program.resolve(module, node.func)
        if name in {"unittest.mock.patch", "mock.patch"} and node.args:
            value = node.args[0]
            if isinstance(value, ast.Constant) and isinstance(value.value, str):
                return value.value
        if (
            name in {"unittest.mock.patch.object", "mock.patch.object"}
            and len(node.args) >= 2
        ):
            attribute = node.args[1]
            if isinstance(attribute, ast.Constant) and isinstance(attribute.value, str):
                return program.resolve(module, node.args[0]) + "." + attribute.value
        return None

    result = {}
    parameters = [p.arg for p in function.args.args if p.arg not in {"self", "cls"}]
    injected = 0
    for index, decorator in enumerate(reversed(function.decorator_list)):
        patched = target(decorator)
        if patched:
            name = "@decorator" + str(index)
            replacement = len(decorator.args) > (
                2 if spelling(decorator.func).endswith(".object") else 1
            )
            replacement |= any(keyword.arg == "new" for keyword in decorator.keywords)
            if not replacement and injected < len(parameters):
                name = parameters[injected]
                injected += 1
            result[name] = patched
    for node in descendants(function):
        if isinstance(node, (ast.With, ast.AsyncWith)):
            for item in node.items:
                patched = target(item.context_expr)
                if patched:
                    result[
                        spelling(item.optional_vars) or "@with" + str(node.lineno)
                    ] = patched
    return result


def known_truth(node, env):
    node = expand(node, env)
    if isinstance(node, ast.Constant):
        return bool(node.value)
    if isinstance(node, ast.UnaryOp) and isinstance(node.op, ast.Not):
        value = known_truth(node.operand, env)
        return None if value is None else not value
    return None


def boundary_reached(program, test, callee, boundary):
    """False for a proved other arm, True for a direct unconditional arm.

    Unresolved wrapper/branch paths remain None, not real-tool coverage.
    """
    module, _, function, _ = program.functions[test]
    producer = program.functions[callee][2]
    calls = [
        node
        for node in descendants(function)
        if isinstance(node, ast.Call) and program.resolve(module, node.func) == callee
    ]
    if not calls:
        return None
    states = []
    for call in calls:
        env = bindings(producer)
        caller_env = bindings(function)
        parameters = producer.args.posonlyargs + producer.args.args
        env.update(
            {p.arg: expand(a, caller_env) for p, a in zip(parameters, call.args)}
        )
        env.update(
            {kw.arg: expand(kw.value, caller_env) for kw in call.keywords if kw.arg}
        )
        state = True
        for branch in descendants(producer):
            if isinstance(branch, ast.If):
                in_body = any(boundary in ast.walk(node) for node in branch.body)
                in_else = any(boundary in ast.walk(node) for node in branch.orelse)
                if in_body or in_else:
                    truth = known_truth(branch.test, env)
                    if truth is None:
                        state = None
                    elif truth != in_body:
                        state = False
                        break
        states.append(state)
    return True if True in states else None if None in states else False


def scripted_fallbacks(program, module, function, checks, env, expected, reached):
    patched = patch_bindings(program, module, function)
    counts = {}
    for check, kind, args in checks:
        if kind in {"assert_called_once", "assert_called_once_with"}:
            counts[spelling(check.func.value)] = 1
        if kind == "assertEqual" and len(args) >= 2:
            for actual, count in [(args[0], args[1]), (args[1], args[0])]:
                if (
                    isinstance(actual, ast.Attribute)
                    and actual.attr == "call_count"
                    and isinstance(count, ast.Constant)
                    and type(count.value) is int
                    and count.value >= 0
                ):
                    counts[spelling(actual.value)] = count.value
    targets = {target for callee in reached for target, _ in effects(program, callee)}
    targets.update(
        program.functions[callee][0] + "." + spelling(call.func)
        for callee in reached
        for _, call in effects(program, callee)
    )
    for node in descendants(function):
        if not isinstance(node, ast.Assign):
            continue
        values = expand(node.value, env)
        if not isinstance(values, (ast.List, ast.Tuple)):
            continue
        for target in node.targets:
            if not isinstance(target, ast.Attribute) or target.attr != "side_effect":
                continue
            name = spelling(target.value)
            count = counts.get(name)
            if patched.get(name) not in targets or count is None:
                continue
            if any(
                isinstance(value, ast.Constant)
                and type(value.value) is type(expected.value)
                and value.value == expected.value
                for value in values.elts[count:]
            ):
                yield name


def closed_oracle(node, env):
    """Only locally constructed values, not an unknown collaborator's output."""
    node = expand(node, env)
    if isinstance(node, ast.Call):
        return (
            spelling(node.func) in {"len", "str", "int", "bool"}
            and not node.keywords
            and all(closed_oracle(arg, env) for arg in node.args)
        )
    if isinstance(node, (ast.Name, ast.Attribute, ast.Subscript, ast.Lambda)):
        return False
    return all(closed_oracle(child, env) for child in ast.iter_child_nodes(node))


def parameter_flow_is_closed(function):
    # Effects and iteration can influence a return without occurring in its
    # value expression (including mutation of another argument or a global).
    return not any(
        isinstance(
            node,
            (
                ast.Call,
                ast.AugAssign,
                ast.NamedExpr,
                ast.For,
                ast.AsyncFor,
                ast.Try,
                ast.With,
                ast.AsyncWith,
                ast.Global,
                ast.Nonlocal,
                ast.Yield,
                ast.YieldFrom,
                ast.Await,
                ast.Delete,
            ),
        )
        or (
            isinstance(node, (ast.Attribute, ast.Subscript))
            and isinstance(node.ctx, ast.Store)
        )
        for node in descendants(function)
    )


def argparse_default(program, module, actual, env):
    actual = expand(actual, env)
    field = None
    if isinstance(actual, ast.Attribute) and isinstance(actual.value, ast.Call):
        call = actual.value
        if (
            isinstance(call.func, ast.Attribute)
            and call.func.attr == "parse_args"
            and len(call.args) == 1
            and isinstance(call.args[0], (ast.List, ast.Tuple))
            and not call.args[0].elts
            and not call.keywords
        ):
            field = actual.attr
    elif isinstance(actual, ast.Call):
        call = actual
        if (
            isinstance(call.func, ast.Attribute)
            and call.func.attr == "get_default"
            and len(call.args) == 1
            and isinstance(call.args[0], ast.Constant)
        ):
            field = call.args[0].value
    if not isinstance(field, str):
        return False
    receiver = expand(call.func.value, env)
    if not isinstance(receiver, ast.Call):
        return False
    callee = program.resolve(module, receiver.func)
    if callee not in program.functions:
        return False
    owner, _, builder, _ = program.functions[callee]
    locals_ = bindings(builder)
    for returned in descendants(builder):
        if not isinstance(returned, ast.Return) or not isinstance(
            returned.value, ast.Name
        ):
            continue
        parser = returned.value.id
        constructor = locals_.get(parser)
        if (
            not isinstance(constructor, ast.Call)
            or program.resolve(owner, constructor.func) != "argparse.ArgumentParser"
        ):
            continue
        calls = [
            n
            for n in descendants(builder)
            if isinstance(n, ast.Call)
            and isinstance(n.func, ast.Attribute)
            and spelling(n.func.value) == parser
        ]
        if any(n.func.attr != "add_argument" for n in calls):
            continue
        for definition in calls:
            keywords = {keyword.arg: keyword.value for keyword in definition.keywords}
            names = [
                arg.value.lstrip("-").replace("-", "_")
                for arg in definition.args
                if isinstance(arg, ast.Constant) and isinstance(arg.value, str)
            ]
            destination = keywords.get("dest")
            if isinstance(destination, ast.Constant):
                names = [destination.value]
            if field in names and isinstance(keywords.get("default"), ast.Constant):
                return True
    return False


def validate_sources(sources):
    """Bound authored input without opening paths or importing project code."""
    if (
        not isinstance(sources, dict)
        or len(sources) > MAX_SOURCE_FILES
        or any(
            not isinstance(path, str) or not isinstance(source, str)
            for path, source in sources.items()
        )
    ):
        raise ValueError("sources must map at most 10000 paths to source strings")
    size = 0
    for path, source in sources.items():
        size += len(path.encode("utf-8")) + len(source.encode("utf-8"))
        if size > MAX_INPUT_BYTES:
            raise ValueError("Python oracle input exceeds 16 MiB")


def unresolved_calls(program):
    """Retain unresolved calls, including top-level dynamic loader activity."""
    for module, (path, tree) in sorted(program.modules.items()):
        for node in ast.walk(tree):
            if not isinstance(node, ast.Call):
                continue
            target = program.resolve(module, node.func)
            if target in program.functions:
                continue
            dynamic_import = (
                target in {"__import__", "builtins.__import__", "eval", "exec"}
                or target.startswith("importlib.")
                or isinstance(node.func, ast.Attribute)
                and node.func.attr in {"exec_module", "load_module", "find_spec"}
            )
            yield {
                "path": path,
                "line": node.lineno,
                "reason": "dynamic_import" if dynamic_import else "unresolved_call",
                "target": target or "<dynamic expression>",
                "detail": "call is not resolved to supplied source; no behavior coverage is inferred",
            }


def analyze(sources: dict[str, str]) -> dict:
    """Return advisory evidence; parsing success is not behavioral completeness."""
    validate_sources(sources)
    program = Program(sources)
    parse_complete = not program.unknown
    program.unknown.extend(unresolved_calls(program))
    findings = []
    tests = {}
    for identity, (module, path, function, is_test) in sorted(
        program.functions.items()
    ):
        if not is_test:
            continue
        reached = program.reached(identity)
        env = bindings(function)
        checks = list(assertions(function))
        patches = patch_targets(program, module, function)
        tests[identity] = {
            "reached": sorted(reached),
            "patches": sorted(patches),
            "content_fingerprint": hashlib.sha256(
                ast.get_source_segment(sources[path], function).encode()
            ).hexdigest(),
        }

        def report(reason, node, detail, callable_=None):
            line = sources[path].splitlines(keepends=True)
            start = (
                sum(len(s.encode()) for s in line[: node.lineno - 1]) + node.col_offset
            )
            end = (
                sum(len(s.encode()) for s in line[: node.end_lineno - 1])
                + node.end_col_offset
            )
            findings.append(
                {
                    "lint_id": "test-without-effective-oracle",
                    "severity": "warn",
                    "path": path,
                    "primary_range": {"start_byte": start, "end_byte": end},
                    "test_callable": identity,
                    "reason": reason,
                    "detail": detail,
                    "callable": callable_,
                    "evidence": ast.get_source_segment(sources[path], node),
                }
            )

        def produced(node):
            node = expand(node, env)
            field = None
            if isinstance(node, ast.Subscript) and isinstance(node.slice, ast.Constant):
                field = node.slice.value
                node = expand(node.value, env)
            if isinstance(node, ast.Call):
                callee = program.resolve(module, node.func)
                if callee in reached:
                    return callee, node, field
            return None

        for node, kind, args in checks:
            expanded = [expand(arg, env) for arg in args]
            provenance = [n for arg in args for n in traced_nodes(arg, env)]
            if any(repository_read(n) for n in provenance):
                report(
                    "repository_text_change_detector",
                    node,
                    "an oracle reads live repository source/config; this is change detection, not behavior coverage",
                )
                tests[identity]["coverage_class"] = "change_detector"

            clocks = {"time.monotonic", "time.perf_counter", "time.time"}
            real_clock = any(
                isinstance(n, ast.Call)
                and program.resolve(module, n.func) in clocks
                and program.resolve(module, n.func) not in patches
                for n in provenance
            )
            thresholds = [
                n.value
                for n in provenance
                if isinstance(n, ast.Constant)
                and type(n.value) in (int, float)
                and 0 < n.value < 1
            ]
            if (
                real_clock
                and thresholds
                and (
                    kind
                    in {
                        "assertLess",
                        "assertLessEqual",
                        "assertGreater",
                        "assertGreaterEqual",
                    }
                    or any(isinstance(n, ast.Compare) for n in provenance)
                )
            ):
                report(
                    "wall_clock_subsecond_oracle",
                    node,
                    "pass/fail compares an unpatched real clock with a sub-second threshold",
                )

            if kind in {"assertEqual", "assertIs"} and len(expanded) >= 2:
                # Compare actual-call arguments substituted into reached return
                # expressions. Tiny literal/name expressions are not clones.
                for actual, expected in [
                    (expanded[0], expanded[1]),
                    (expanded[1], expanded[0]),
                ]:
                    output = produced(actual)
                    if not output or output[2] is not None:
                        continue
                    callee, call, _ = output
                    target_function = program.functions[callee][2]
                    parameters = (
                        target_function.args.posonlyargs + target_function.args.args
                    )
                    if call.keywords or len(call.args) != len(parameters):
                        continue
                    _, flow_env = program.returns(callee)
                    flow_env.update(
                        {p.arg: expand(a, env) for p, a in zip(parameters, call.args)}
                    )
                    key = expression_key(expected, env)
                    candidates = [
                        n.value
                        for n in descendants(target_function)
                        if isinstance(n, ast.Return) and n.value
                    ]
                    conditions = [
                        n.test
                        for n in descendants(target_function)
                        if isinstance(n, (ast.If, ast.IfExp, ast.While)) and n.test
                    ]
                    copied_return = any(
                        sum(1 for _ in ast.walk(candidate)) >= 8
                        and key == expression_key(candidate, flow_env)
                        for candidate in candidates
                    )
                    copied_condition = any(
                        sum(1 for _ in ast.walk(condition)) >= 5
                        and key == expression_key(condition, flow_env)
                        for condition in conditions
                    )
                    if copied_return or copied_condition:
                        report(
                            "expectation_derived_from_production",
                            node,
                            "expected expression copies a reached production return or branch condition with the same inputs",
                            callee,
                        )
                for actual, expected in [
                    (expanded[0], expanded[1]),
                    (expanded[1], expanded[0]),
                ]:
                    if not isinstance(expected, ast.Constant):
                        continue
                    target = program.resolve(module, actual)
                    reflection = (
                        isinstance(actual, ast.Attribute)
                        and actual.attr == "default"
                        and any(
                            isinstance(call, ast.Call)
                            and program.resolve(module, call.func)
                            == "inspect.signature"
                            for call in ast.walk(actual)
                        )
                    )
                    if (
                        target in program.constants
                        or reflection
                        or argparse_default(program, module, actual, env)
                    ):
                        report(
                            "constant_only_oracle",
                            node,
                            "literal compared with a module constant or reflected signature default",
                        )
                    output = produced(actual)
                    if output and any(
                        scripted_fallbacks(
                            program, module, function, checks, env, expected, reached
                        )
                    ):
                        report(
                            "scripted_fallback_matches_expected",
                            node,
                            "a scripted response beyond the asserted call count supplies the expected answer; only call-count evidence distinguishes that extra-request path",
                            output[0],
                        )
                    if output and output[2] is not None:
                        fields = program.output_fields(output[0])
                        values = fields.get(output[2], []) if fields is not None else []
                        if values and all(
                            isinstance(value, ast.Constant)
                            and value.value == expected.value
                            for value in values
                        ):
                            report(
                                "literal_output_self_description",
                                node,
                                "every closed return assigns this field the asserted literal",
                                output[0],
                            )
                left, right = produced(expanded[0]), produced(expanded[1])
                if left and right and left[0] == right[0] and left[2] == right[2]:
                    a, b = left[1], right[1]
                    if len(a.args) == len(b.args) and not a.keywords and not b.keywords:
                        changed = [
                            i
                            for i, (x, y) in enumerate(zip(a.args, b.args))
                            if normalized(x) != normalized(y)
                        ]
                        function_ = program.functions[left[0]][2]
                        parameters = function_.args.posonlyargs + function_.args.args
                        if len(changed) == 1 and changed[0] < len(parameters):
                            parameter = parameters[changed[0]].arg
                            fields = program.output_fields(left[0])
                            values = (
                                fields.get(left[2], []) if fields is not None else []
                            )
                            flow_env = bindings(function_)
                            inputs = [value_inputs(value, flow_env) for value in values]
                            parameter_names = {p.arg for p in parameters}
                            if (
                                values
                                and parameter_flow_is_closed(function_)
                                and all(
                                    names is not None
                                    and names <= parameter_names
                                    and parameter not in names
                                    for names in inputs
                                )
                            ):
                                # Branches can control a return without appearing in its value.
                                conditions = [
                                    n.test
                                    for n in descendants(function_)
                                    if isinstance(n, (ast.If, ast.While, ast.IfExp))
                                ]
                                control_inputs = [
                                    value_inputs(cond, flow_env) for cond in conditions
                                ]
                                if all(
                                    names is not None and parameter not in names
                                    for names in control_inputs
                                ):
                                    report(
                                        "compared_value_independent_of_parameter",
                                        node,
                                        "compared field does not depend on differing parameter "
                                        + parameter,
                                        left[0],
                                    )
            if (
                kind == "assertNotIn"
                and len(expanded) >= 2
                and isinstance(expanded[0], ast.Constant)
            ):
                output = produced(expanded[1])
                if output:
                    fields = program.output_fields(output[0])
                    if fields is not None and expanded[0].value not in fields:
                        report(
                            "impossible_absence_oracle",
                            node,
                            "closed production output cannot contain the excluded key",
                            output[0],
                        )

        helper_checks = any(
            list(assertions(program.functions[callee][2])) for callee in reached
        )
        if reached and not checks and not helper_checks:
            report(
                "no_effective_oracle",
                function,
                "reached code has no executed assertion in the statically resolved test closure",
            )
        elif (
            reached
            and checks
            and not helper_checks
            and not patches
            and all(
                args and all(closed_oracle(arg, env) for arg in args)
                for _, _, args in checks
            )
        ):
            report(
                "oracle_without_production_flow",
                checks[0][0],
                "all checked values are constructed locally; none flows from reached production",
            )

        # A mock call check is not behavior coverage. Require every assertion to
        # be a call-argument oracle and a patched boundary in reached production.
        if (
            checks
            and all(
                kind
                in {"assert_called_with", "assert_called_once_with", "assert_has_calls"}
                for _, kind, _ in checks
            )
            and patches
        ):
            matched = []
            for callee in reached:
                owner = program.functions[callee][0]
                for target, call in effects(program, callee):
                    local_targets = {owner + "." + spelling(call.func), target}
                    if patches & local_targets:
                        for check, _, args in checks:
                            if (
                                [normalized(arg) for arg in args]
                                == [normalized(arg) for arg in call.args]
                                and isinstance(check, ast.Call)
                                and [normalized(k) for k in check.keywords]
                                == [normalized(k) for k in call.keywords]
                            ):
                                matched.append(callee)
            if matched:
                report(
                    "mock_call_site_only_oracle",
                    checks[0][0],
                    "only mock argument equalities repeat a reached patched call site",
                    sorted(matched)[0],
                )

    # Cross-test boundary evidence: one real reaching test discharges the
    # mocked-only claim for that exact external call (including its options).
    for callee, (module, path, function, is_test) in sorted(program.functions.items()):
        if is_test:
            continue
        reaching = [
            (test, facts) for test, facts in tests.items() if callee in facts["reached"]
        ]
        for target, call in effects(program, callee):
            aliases = {module + "." + spelling(call.func), target}
            scoped = [
                (test, facts, boundary_reached(program, test, callee, call))
                for test, facts in reaching
            ]
            scoped = [
                (test, facts, state)
                for test, facts, state in scoped
                if state is not False
            ]
            unknown_tests = [test for test, _, state in scoped if state is None]
            for test in unknown_tests:
                program.unknown.append(
                    {
                        "path": path,
                        "line": call.lineno,
                        "test_callable": test,
                        "callable": callee,
                        "reason": "boundary_reachability_unknown",
                        "detail": "wrapper or branch reachability is unresolved; not real boundary coverage",
                    }
                )
            mocked = [
                (test, facts, state)
                for test, facts, state in scoped
                if aliases & set(facts["patches"])
            ]
            real = any(
                state is True and not aliases & set(facts["patches"])
                for _, facts, state in scoped
            )
            if mocked and not real:
                findings.append(
                    {
                        "lint_id": "test-without-effective-oracle",
                        "severity": "warn",
                        "path": path,
                        "test_callable": mocked[0][0],
                        "reason": "external_boundary_only_mocked",
                        "callable": callee,
                        "detail": "patched tests reach "
                        + target
                        + "; no statically established unpatched reaching test",
                        "evidence": ast.get_source_segment(sources[path], call),
                        "reaching_tests": [test for test, _, _ in scoped],
                        "unknown_reaching_tests": unknown_tests,
                    }
                )
    findings.sort(
        key=lambda item: (
            item["path"],
            item["test_callable"],
            item["reason"],
            item.get("evidence") or "",
        )
    )
    return {
        "schema_version": 1,
        "language": "python",
        "diagnostics": findings,
        "tests": tests,
        "parse_complete": parse_complete,
        "analysis_complete": False,
        "advisory_only": True,
        "unknown": program.unknown,
        "limitations": [
            "heuristic advisory analysis never establishes behavioral completeness; analysis_complete is always false",
            "findings may flag intentional compatibility/change-detection contracts; human review is required",
            "static direct/imported call graph; dynamic dispatch, dynamic imports and runtime monkeypatching are unresolved",
            "closed literal return dictionaries only for field/absence/parameter proofs",
        ],
    }


def main():
    try:
        payload = sys.stdin.buffer.read(MAX_INPUT_BYTES + 1)
        if len(payload) > MAX_INPUT_BYTES:
            raise ValueError("Python oracle input exceeds 16 MiB")
        request = json.loads(payload)
        if not isinstance(request, dict) or "sources" not in request:
            raise ValueError("request must contain a sources mapping")
        report = analyze(request["sources"])
    except (ValueError, TypeError, UnicodeError, RecursionError) as error:
        print(f"Python oracle input/analysis error: {error}", file=sys.stderr)
        return 2
    json.dump(report, sys.stdout, sort_keys=True)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())

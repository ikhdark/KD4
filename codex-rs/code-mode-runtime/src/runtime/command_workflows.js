// Mechanical command composition. Native tools still own selection, policies,
// process lifecycle and result provenance.
(() => {
  const record = (value) => value !== null && typeof value === "object" && !Array.isArray(value);
  const requiredString = (value) => typeof value === "string" && value.trim() !== "";
  Object.defineProperty(globalThis, "command_scope", {
    value: function command_scope(defaults) {
      if (!record(defaults) || !requiredString(defaults.workdir) ||
          Object.keys(defaults).some((key) => !["workdir", "environment_id", "env"].includes(key)) ||
          (defaults.environment_id !== undefined && !requiredString(defaults.environment_id)) ||
          (defaults.env !== undefined && !record(defaults.env)))
        throw new TypeError("command_scope requires workdir, optional environment_id and optional env");
      const scoped = { ...defaults };
      if (scoped.env !== undefined) scoped.env = { ...scoped.env };
      return (request, options) => {
        if (!record(request)) throw new TypeError("expected exec_command arguments");
        return resolve_tool("exec_command")({ ...scoped, ...request }, options);
      };
    },
  });

  Object.defineProperty(globalThis, "exec_glob", {
    value: async function exec_glob(command, files) {
      if (!record(command) || !requiredString(command.program) ||
          (command.args !== undefined && (!Array.isArray(command.args) ||
            command.args.some((argument) => typeof argument !== "string"))) ||
          ["workdir", "cmd", "script_body", "shell", "kind"].some((key) => key in command))
        throw new TypeError("exec_glob requires program/args in the selected environment cwd");
      const request = { ...command, args: [...(command.args ?? [])] };
      if (!record(files) || !requiredString(files.path) ||
          !Array.isArray(files.include_paths) || files.include_paths.length === 0 ||
          Object.keys(files).some((key) => ![
            "path", "include_paths", "exclude_paths", "exclude_directory_paths",
            "include_hidden", "include_ignored", "max_entries", "max_depth", "max_directories",
          ].includes(key)) ||
          (files.max_entries !== undefined &&
            (!Number.isInteger(files.max_entries) || files.max_entries < 1 || files.max_entries > 128)))
        throw new TypeError("exec_glob requires path, include_paths and at most 128 matching files");
      const listing = await resolve_tool("list_files")({
        max_entries: 128, ...files, environment_id: request.environment_id,
      });
      const fail = (message) => { throw Object.assign(new Error(message), { evidence: listing }); };
      if (listing.complete !== true || listing.truncated !== false ||
          !Array.isArray(listing.errors) || listing.errors.length !== 0 ||
          !Array.isArray(listing.entries) || typeof listing.environment_id !== "string" ||
          (request.environment_id !== undefined && listing.environment_id !== request.environment_id))
        fail("glob expansion was incomplete; no command launched");
      if (listing.entries.length === 0)
        fail("glob matched no files; no command launched");
      if (listing.entries.length > 128 || listing.entries.some((entry) =>
        !entry || !(entry.kind === "file" || entry.kind === "symlink") ||
        !requiredString(entry.path) || entry.path.includes("\u0000")))
        fail("glob contained unaddressable entries; no command launched");
      const paths = listing.entries.map((entry) =>
        entry.path.startsWith("-") ? "./" + entry.path : entry.path).sort();
      return resolve_tool("exec_command")({
        ...request, environment_id: listing.environment_id, args: [...request.args, ...paths],
      });
    },
  });
})();

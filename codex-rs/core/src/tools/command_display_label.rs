const KDWIREGUARD_DISPLAY_LABEL: &str = "KDWireGuard -";

pub(crate) fn for_command(command: &[String]) -> Option<String> {
    is_kdwireguard_command(command).then(|| KDWIREGUARD_DISPLAY_LABEL.to_string())
}

fn is_kdwireguard_command(command: &[String]) -> bool {
    if is_wrapper_argv(command) {
        return true;
    }

    shell_script(command).is_some_and(is_wrapper_script)
}

fn is_wrapper_argv(command: &[String]) -> bool {
    matches!(
        command,
        [executable, agent, ..] if is_wrapper_executable(executable) && agent == "--agent"
    )
}

fn shell_script(command: &[String]) -> Option<&str> {
    if command.len() == 1 {
        return command.first().map(String::as_str);
    }

    let shell = executable_basename(command.first()?);
    if !is_shell_executable(shell) {
        return None;
    }

    command
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, argument)| is_shell_command_flag(argument))
        .and_then(|(index, _)| command.get(index + 1))
        .map(String::as_str)
}

fn executable_basename(value: &str) -> &str {
    value
        .trim_matches(|character: char| {
            character.is_whitespace() || matches!(character, '\'' | '"' | '(' | ')' | '[' | ']')
        })
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
}

fn is_wrapper_executable(value: &str) -> bool {
    matches!(
        executable_basename(value).to_ascii_lowercase().as_str(),
        "kds" | "kds.exe"
    )
}

fn is_shell_executable(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    matches!(
        value.as_str(),
        "fish"
            | "fish.exe"
            | "nu"
            | "nu.exe"
            | "powershell"
            | "powershell.exe"
            | "pwsh"
            | "pwsh.exe"
            | "cmd"
            | "cmd.exe"
            | "xonsh"
            | "xonsh.exe"
    ) || value.ends_with("sh")
        || value.ends_with("sh.exe")
}

fn is_shell_command_flag(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "-c" | "-lc" | "/c" | "-command" | "-commandwithargs"
    )
}

fn is_wrapper_script(script: &str) -> bool {
    command_segments(script).into_iter().any(|segment| {
        let executable_index = segment
            .first()
            .is_some_and(|token| is_command_prefix(token))
            .then_some(1)
            .unwrap_or(0);

        segment.get(executable_index..).is_some_and(is_wrapper_argv)
    })
}

fn is_command_prefix(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "call" | "command" | "exec"
    )
}

fn command_segments(script: &str) -> Vec<Vec<String>> {
    let script = script
        .trim_start()
        .strip_prefix("\"\"")
        .map(|rest| format!("\"{rest}"))
        .unwrap_or_else(|| script.to_string());
    let mut segments = Vec::new();
    let mut segment = Vec::new();
    let mut token = String::new();
    let mut quote = None;
    let mut escaped = false;

    let mut characters = script.chars().peekable();
    while let Some(character) = characters.next() {
        if escaped {
            token.push(character);
            escaped = false;
            continue;
        }

        if quote != Some('\'') && character == '`' {
            escaped = true;
            continue;
        }

        if let Some(active_quote) = quote {
            if character == active_quote {
                quote = None;
            } else {
                token.push(character);
            }
            continue;
        }

        match character {
            '\'' | '"' => quote = Some(character),
            '\\' if characters
                .peek()
                .is_some_and(|next| next.is_whitespace() || matches!(*next, '\'' | '"')) =>
            {
                escaped = true;
            }
            '^' if characters.peek().is_some() => escaped = true,
            '\\' => token.push(character),
            character if character.is_whitespace() => push_token(&mut segment, &mut token),
            ';' | '&' | '|' | '<' | '>' => {
                push_token(&mut segment, &mut token);
                push_segment(&mut segments, &mut segment);
            }
            _ => token.push(character),
        }
    }

    push_token(&mut segment, &mut token);
    push_segment(&mut segments, &mut segment);
    segments
}

fn push_token(segment: &mut Vec<String>, token: &mut String) {
    if !token.is_empty() {
        segment.push(std::mem::take(token));
    }
}

fn push_segment(segments: &mut Vec<Vec<String>>, segment: &mut Vec<String>) {
    if !segment.is_empty() {
        segments.push(std::mem::take(segment));
    }
}

#[cfg(test)]
#[path = "command_display_label_tests.rs"]
mod tests;

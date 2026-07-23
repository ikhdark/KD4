const ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;

#[derive(Clone, Copy)]
enum VirtualTerminalInput {
    Enabled,
    Disabled,
}

#[derive(Clone, Copy)]
struct InputModeSnapshot {
    original: VirtualTerminalInput,
    restore_failed: bool,
}

fn input_record_mode(mode: u32) -> u32 {
    mode & !ENABLE_VIRTUAL_TERMINAL_INPUT
}

fn restored_input_mode(mode: u32, original: VirtualTerminalInput) -> u32 {
    match original {
        VirtualTerminalInput::Enabled => mode | ENABLE_VIRTUAL_TERMINAL_INPUT,
        VirtualTerminalInput::Disabled => input_record_mode(mode),
    }
}

static ORIGINAL_VT_INPUT: std::sync::Mutex<Vec<InputModeSnapshot>> =
    std::sync::Mutex::new(Vec::new());

fn current_input_mode() -> std::io::Result<Option<(windows_sys::Win32::Foundation::HANDLE, u32)>> {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::GetConsoleMode;
    use windows_sys::Win32::System::Console::GetStdHandle;
    use windows_sys::Win32::System::Console::STD_INPUT_HANDLE;

    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    if handle == 0 {
        return Ok(None);
    }

    let mut mode = 0;
    if unsafe { GetConsoleMode(handle, &mut mode) } == 0 {
        return Err(std::io::Error::last_os_error());
    }

    Ok(Some((handle, mode)))
}

pub(super) fn set_input_record_mode() -> std::io::Result<()> {
    use windows_sys::Win32::System::Console::SetConsoleMode;

    let mut snapshots = ORIGINAL_VT_INPUT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some((handle, mode)) = current_input_mode()? else {
        return Ok(());
    };
    let requested_mode = input_record_mode(mode);
    if requested_mode != mode && unsafe { SetConsoleMode(handle, requested_mode) } == 0 {
        return Err(std::io::Error::last_os_error());
    }

    let original = if mode & ENABLE_VIRTUAL_TERMINAL_INPUT != 0 {
        VirtualTerminalInput::Enabled
    } else {
        VirtualTerminalInput::Disabled
    };
    if snapshots
        .last()
        .is_some_and(|snapshot| snapshot.restore_failed)
    {
        return Ok(());
    }
    snapshots.push(InputModeSnapshot {
        original,
        restore_failed: false,
    });
    Ok(())
}

pub(super) fn ensure_input_record_mode() -> std::io::Result<()> {
    use windows_sys::Win32::System::Console::SetConsoleMode;

    let snapshots = ORIGINAL_VT_INPUT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if snapshots.is_empty() {
        return Ok(());
    }
    let Some((handle, mode)) = current_input_mode()? else {
        return Ok(());
    };
    let requested_mode = input_record_mode(mode);
    if requested_mode != mode && unsafe { SetConsoleMode(handle, requested_mode) } == 0 {
        return Err(std::io::Error::last_os_error());
    }

    Ok(())
}

pub(super) fn restore_input_mode() -> std::io::Result<()> {
    use windows_sys::Win32::System::Console::SetConsoleMode;

    let mut original_modes = ORIGINAL_VT_INPUT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(snapshot) = original_modes.last_mut() else {
        return Ok(());
    };
    let original = snapshot.original;
    let Some((handle, mode)) = (match current_input_mode() {
        Ok(current) => current,
        Err(err) => {
            snapshot.restore_failed = true;
            return Err(err);
        }
    }) else {
        snapshot.restore_failed = true;
        return Ok(());
    };
    let requested_mode = restored_input_mode(mode, original);
    if requested_mode != mode && unsafe { SetConsoleMode(handle, requested_mode) } == 0 {
        snapshot.restore_failed = true;
        return Err(std::io::Error::last_os_error());
    }

    original_modes.pop();
    Ok(())
}

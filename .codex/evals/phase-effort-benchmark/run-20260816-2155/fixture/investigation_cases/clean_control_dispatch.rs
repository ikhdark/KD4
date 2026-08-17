#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Command {
    Start,
    Stop,
}

#[derive(Debug, Default)]
struct State {
    running: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TransitionError {
    AlreadyRunning,
    AlreadyStopped,
}

fn apply(state: &mut State, command: Command) -> Result<(), TransitionError> {
    match (command, state.running) {
        (Command::Start, false) => {
            state.running = true;
            Ok(())
        }
        (Command::Stop, true) => {
            state.running = false;
            Ok(())
        }
        (Command::Start, true) => Err(TransitionError::AlreadyRunning),
        (Command::Stop, false) => Err(TransitionError::AlreadyStopped),
    }
}

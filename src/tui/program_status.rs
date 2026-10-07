use std::io::{self, Write};

use tuika::term::program_status::{self, BlockKind, Report, State};

/// Reports yolop's top-level lifecycle to terminals that support OSC 7501.
pub(crate) struct ProgramStatus<W: Write = io::Stdout> {
    writer: W,
}

impl ProgramStatus<io::Stdout> {
    pub(crate) fn stdout() -> Self {
        Self::new(io::stdout())
    }
}

impl<W: Write> ProgramStatus<W> {
    fn new(writer: W) -> Self {
        Self { writer }
    }

    fn write(&mut self, report: Report) {
        let _ = program_status::write(&mut self.writer, &report);
    }

    pub(crate) fn working(&mut self) {
        self.write(Report::new(State::Working));
    }

    pub(crate) fn blocked_permission(&mut self) {
        self.write(Report::new(State::Blocked).kind(BlockKind::Permission));
    }

    pub(crate) fn blocked_question(&mut self) {
        self.write(Report::new(State::Blocked).kind(BlockKind::Question));
    }

    pub(crate) fn done(&mut self) {
        self.write(Report::new(State::Done));
    }

    pub(crate) fn error(&mut self) {
        self.write(Report::new(State::Error));
    }

    pub(crate) fn clear(&mut self) {
        self.write(Report::clear(None));
    }
}

impl<W: Write> Drop for ProgramStatus<W> {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::ProgramStatus;

    #[test]
    fn emits_exact_osc_for_every_status() {
        let mut output = Vec::new();
        {
            let mut status = ProgramStatus::new(&mut output);
            status.working();
            status.blocked_permission();
            status.blocked_question();
            status.done();
            status.error();
            status.clear();
        }

        assert_eq!(
            output,
            b"\x1b]7501;state=working\x1b\\\x1b]7501;state=blocked:kind=permission\x1b\\\x1b]7501;state=blocked:kind=question\x1b\\\x1b]7501;state=done\x1b\\\x1b]7501;state=error\x1b\\\x1b]7501;state=clear\x1b\\\x1b]7501;state=clear\x1b\\"
        );
    }

    #[test]
    fn clears_status_on_drop() {
        let mut output = Vec::new();
        {
            let mut status = ProgramStatus::new(&mut output);
            status.working();
        }

        assert_eq!(
            output,
            b"\x1b]7501;state=working\x1b\\\x1b]7501;state=clear\x1b\\"
        );
    }
}

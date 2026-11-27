//! On-disk checkpoint pipe identities shared by the guest and the host CLI.
use std::io;
use std::num::NonZeroU32;

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CheckpointStdio {
    pub(crate) tty: bool,
    #[serde(default)]
    pub(crate) close_after_input: bool,
    pub(crate) stdin: Option<NonZeroU32>,
    pub(crate) stdout: Option<NonZeroU32>,
    pub(crate) stderr: Option<NonZeroU32>,
}

impl CheckpointStdio {
    pub(crate) fn validate(&self) -> io::Result<()> {
        if self.close_after_input && self.stdin.is_none() { return Err(io::Error::other("checkpoint input close without a pipe")); }
        let pipes = [self.stdin, self.stdout, self.stderr];
        for (i, pipe) in pipes.iter().enumerate() {
            if pipe.is_some() && (self.tty || pipes[..i].contains(pipe)) {
                return Err(io::Error::other("invalid checkpoint engine pipe identities"));
            }
        }
        Ok(())
    }
}

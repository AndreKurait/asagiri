//! Windows dashboard context selection, adapted from libracoder/kiri#828.
//! Docker owns its configuration and retains existing context endpoints.
use std::io;

pub fn select(
    name: &str,
    ours: &str,
    host: &str,
    run: impl Fn(&[&str]) -> io::Result<String>,
) -> io::Result<()> {
    if name.is_empty()
        || name.starts_with('-')
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.+-".contains(&b))
        || matches!(name, "." | "..")
    {
        return Err(io::Error::other("not a Docker context name"));
    }
    if name == ours {
        // Docker lists the current context even if its metadata is missing.
        // Make the built-in default current for this read alone.
        let names = run(&[
            "--context", "default", "context", "ls", "--format", "{{.Name}}",
        ])?;
        if !names.lines().any(|existing| existing == ours) {
            run(&[
                "context",
                "create",
                ours,
                "--description",
                "Asagiri (WSL2)",
                "--docker",
                &format!("host={host}"),
            ])?;
        }
    }
    run(&["context", "use", name]).map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[test]
    fn recreates_only_the_missing_owned_context() {
        for (name, existing, commands) in [
            ("asagiri-dev", "default\n", vec!["ls", "create", "use"]),
            ("asagiri-dev", "default\nasagiri-dev\n", vec!["ls", "use"]),
            ("desktop-linux", "", vec!["use"]),
            ("team+dev", "", vec!["use"]),
        ] {
            let seen = RefCell::new(Vec::new());
            select(name, "asagiri-dev", "npipe:////./pipe/dev", |args| {
                if args == ["--context", "default", "context", "ls", "--format", "{{.Name}}"] {
                    seen.borrow_mut().push("ls".to_owned());
                    return Ok(existing.to_owned());
                }
                assert_eq!(args[0], "context");
                if args[1] == "create" {
                    assert_eq!(args[2], "asagiri-dev");
                    assert_eq!(args.last(), Some(&"host=npipe:////./pipe/dev"));
                }
                assert_ne!(args[1], "ls");
                seen.borrow_mut().push(args[1].to_owned());
                Ok(String::new())
            })
            .unwrap();
            assert_eq!(*seen.borrow(), commands);
        }
    }

    #[test]
    fn retries_selection_after_creation_succeeded() {
        let created = Cell::new(false);
        let fail_use = Cell::new(true);
        let run = |args: &[&str]| match args {
            ["--context", "default", "context", "ls", "--format", "{{.Name}}"] => {
                let names = if created.get() {
                    "default\nasagiri\n"
                } else {
                    "default\n"
                };
                Ok(names.to_owned())
            }
            ["context", "create", "asagiri", ..] => {
                assert!(!created.replace(true));
                Ok(String::new())
            }
            ["context", "use", "asagiri"] => {
                assert!(created.get());
                if fail_use.replace(false) {
                    Err(io::Error::other("cannot write Docker config"))
                } else {
                    Ok(String::new())
                }
            }
            _ => panic!("unexpected Docker arguments {args:?}"),
        };
        let host = "npipe:////./pipe/asagiri";
        let err = select("asagiri", "asagiri", host, run).unwrap_err();
        assert_eq!(err.to_string(), "cannot write Docker config");
        select("asagiri", "asagiri", host, run).unwrap();
    }
}

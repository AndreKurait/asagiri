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
        let names = run(&["context", "ls", "--format", "{{.Name}}"])?;
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
    use std::cell::RefCell;

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
                seen.borrow_mut().push(args[1].to_owned());
                if args[1] == "create" {
                    assert_eq!(args[2], "asagiri-dev");
                    assert_eq!(args.last(), Some(&"host=npipe:////./pipe/dev"));
                }
                Ok(existing.to_owned())
            })
            .unwrap();
            assert_eq!(*seen.borrow(), commands);
        }
    }
}

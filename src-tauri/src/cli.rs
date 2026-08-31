use std::process::ExitCode;

use uuid::Uuid;

use crate::{
    model::{DocumentReference, MessageKind},
    storage,
};

const USAGE: &str = r#"Usage:
  scribe say <text> [--ref-heading <A>B>] [--ref-snippet <text>] [--file <path[:line[-end]]>]...
  scribe ack <text> [--file <path[:line[-end]]>]...
  scribe decision <text> --id <id> [--ref-heading <A>B>] [--ref-snippet <text>] [--file <path[:line[-end]]>]...
  scribe unlink <message-id>
  scribe read [<message-id>]

Set the active call in .scribe.json, for example {"call":"retry-placement"}.
Set SCRIBE_NOTES to override it with an explicit markdown path."#;

pub fn is_cli_invocation(args: &[String]) -> bool {
    args.first()
        .is_some_and(|argument| !argument.starts_with("-psn_"))
}

pub fn run(args: Vec<String>) -> ExitCode {
    match execute(args) {
        Ok(message) => {
            if !message.is_empty() {
                println!("{message}");
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("scribe: {error}");
            ExitCode::from(1)
        }
    }
}

fn execute(args: Vec<String>) -> Result<String, String> {
    let Some(command) = args.first().map(String::as_str) else {
        return Err(USAGE.to_string());
    };
    if matches!(command, "help" | "--help" | "-h") {
        return Ok(USAGE.to_string());
    }

    let session = storage::resolve_session()?;
    match command {
        "say" | "ack" | "decision" => post(&session, command, &args[1..]),
        "unlink" => {
            let id = exactly_one(&args[1..], "unlink requires one message ID")?;
            storage::unlink(&session, id)?;
            Ok(format!("unlinked {id}"))
        }
        "read" => {
            if args.len() > 2 {
                return Err("read accepts at most one message ID".to_string());
            }
            let id = args.get(1).map(String::as_str);
            storage::mark_cli_read(&session, id)?;
            Ok(match id {
                Some(id) => format!("marked {id} read"),
                None => "marked all messages read".to_string(),
            })
        }
        _ => Err(format!("unknown command: {command}\n\n{USAGE}")),
    }
}

fn exactly_one<'a>(args: &'a [String], error: &str) -> Result<&'a str, String> {
    if args.len() != 1 {
        return Err(error.to_string());
    }
    Ok(&args[0])
}

fn post(session: &storage::SessionPaths, command: &str, args: &[String]) -> Result<String, String> {
    let Some(text) = args.first() else {
        return Err(format!("{command} requires message text"));
    };
    if text.starts_with("--") {
        return Err(format!("{command} requires message text before options"));
    }

    let mut id = None;
    let mut heading = None;
    let mut snippet = None;
    let mut files = Vec::new();
    let mut index = 1;
    while index < args.len() {
        let flag = args[index].as_str();
        let value = args
            .get(index + 1)
            .ok_or_else(|| format!("{flag} requires a value"))?
            .clone();
        match flag {
            "--id" => id = Some(value),
            "--ref-heading" => heading = Some(value),
            "--ref-snippet" => snippet = Some(value),
            "--file" => files.push(value),
            _ => return Err(format!("unknown option for {command}: {flag}")),
        }
        index += 2;
    }

    let reference = match (heading, snippet) {
        (Some(heading), Some(snippet)) => {
            let heading = heading
                .split('>')
                .map(str::trim)
                .map(str::to_string)
                .collect::<Vec<_>>();
            if heading.is_empty() || heading.iter().any(String::is_empty) || snippet.is_empty() {
                return Err("document reference heading and snippet cannot be empty".to_string());
            }
            Some(DocumentReference { heading, snippet })
        }
        (None, None) => None,
        _ => return Err("--ref-heading and --ref-snippet must be supplied together".to_string()),
    };

    let (kind, message_id) = match command {
        "say" => {
            if id.is_some() {
                return Err("--id is only valid for decisions".to_string());
            }
            (MessageKind::Message, Uuid::new_v4().to_string())
        }
        "ack" => {
            if reference.is_some() {
                return Err("ack messages cannot carry a document reference".to_string());
            }
            if id.is_some() {
                return Err("--id is only valid for decisions".to_string());
            }
            (MessageKind::Ack, Uuid::new_v4().to_string())
        }
        "decision" => {
            let id = id.ok_or_else(|| "decision requires --id <id>".to_string())?;
            if id.trim().is_empty() || id.chars().any(char::is_whitespace) {
                return Err("decision ID must be non-empty and contain no whitespace".to_string());
            }
            (MessageKind::Decision, id)
        }
        _ => unreachable!(),
    };

    let message = storage::make_message(
        session,
        message_id.clone(),
        kind,
        text.clone(),
        reference,
        &files,
    )?;
    storage::append_message(session, &message)?;
    Ok(format!("posted {command} {message_id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_only_cli_subcommands() {
        assert!(is_cli_invocation(&["say".to_string()]));
        assert!(is_cli_invocation(&["read".to_string()]));
        assert!(is_cli_invocation(&["typo".to_string()]));
        assert!(!is_cli_invocation(&["-psn_0_123".to_string()]));
        assert!(!is_cli_invocation(&[]));
    }
}

use std::{io, path::Path};

struct Argument {
  value: String,
  quoted: bool,
}

// Parse an Exec value after the Desktop Entry string-value escapes have been
// decoded by freedesktop-desktop-entry. This deliberately does not use that
// crate's parse_exec helper: version 0.8.1 splits on whitespace and leaves the
// specification's quoting and escaping rules unimplemented.
pub(crate) fn parse_exec(exec: &str, name: &str, icon: Option<&str>, desktop_file: &Path) -> io::Result<Vec<String>> {
  if !exec.is_ascii() {
    return Err(invalid(
      "Exec must contain only ASCII characters before field-code expansion",
    ));
  }
  if exec.contains('\0') {
    return Err(invalid("Exec contains a NUL byte"));
  }

  let mut file_code_seen = false;
  let mut expanded = Vec::new();

  for argument in tokenize(exec)? {
    if argument.quoted {
      expanded.push(expand_quoted_percent(&argument.value)?);
      continue;
    }

    if argument.value == "%i" {
      if let Some(icon) = icon.filter(|icon| !icon.is_empty()) {
        expanded.push("--icon".to_string());
        expanded.push(icon.to_string());
      }
      continue;
    }

    let mut output = String::new();
    let bytes = argument.value.as_bytes();
    let mut index = 0;

    while index < bytes.len() {
      if bytes[index] != b'%' {
        output.push(bytes[index] as char);
        index += 1;
        continue;
      }

      let code = *bytes
        .get(index + 1)
        .ok_or_else(|| invalid("a literal percent sign in Exec must be written as %%"))?;
      index += 2;

      match code {
        b'%' => output.push('%'),
        b'f' | b'u' => mark_file_code(&mut file_code_seen)?,
        b'F' | b'U' => {
          mark_file_code(&mut file_code_seen)?;
          if argument.value.len() != 2 {
            return Err(invalid(format!(
              "%{} may only be used as a complete argument",
              code as char
            )));
          }
        },
        b'i' => {
          return Err(invalid("%i may only be used as a complete argument"));
        },
        b'c' => output.push_str(name),
        b'k' => output.push_str(&desktop_file.to_string_lossy()),
        // Deprecated field codes are removed from the command line.
        b'd' | b'D' | b'n' | b'N' | b'v' | b'm' => {},
        code if code.is_ascii_alphabetic() => {
          return Err(invalid(format!("unknown Exec field code %{}", code as char)));
        },
        _ => return Err(invalid("a literal percent sign in Exec must be written as %%")),
      }
    }

    // A field code that expands to no value also removes an otherwise-empty
    // argument. Explicitly quoted empty arguments remain intact above.
    if !output.is_empty() {
      expanded.push(output);
    }
  }

  let executable = expanded
    .first()
    .ok_or_else(|| invalid("Exec does not contain an executable"))?;
  if executable.is_empty() {
    return Err(invalid("the Exec executable is empty"));
  }
  if executable.contains('=') {
    return Err(invalid("the Exec executable may not contain '='"));
  }

  Ok(expanded)
}

// greetd 0.10.x joins StartSession.cmd with spaces and runs it through
// `/bin/sh -c`. Quote every parsed argument so that this join preserves the
// exact Desktop Entry argv and cannot reinterpret metacharacters.
pub(crate) fn shell_join(arguments: &[String]) -> String {
  arguments
    .iter()
    .map(|argument| {
      let mut quoted = String::with_capacity(argument.len() + 2);
      quoted.push('\'');
      for character in argument.chars() {
        if character == '\'' {
          quoted.push_str("'\\''");
        } else {
          quoted.push(character);
        }
      }
      quoted.push('\'');
      quoted
    })
    .collect::<Vec<_>>()
    .join(" ")
}

fn tokenize(exec: &str) -> io::Result<Vec<Argument>> {
  let bytes = exec.as_bytes();
  let mut arguments = Vec::new();
  let mut index = 0;

  while index < bytes.len() {
    while bytes.get(index) == Some(&b' ') {
      index += 1;
    }
    if index == bytes.len() {
      break;
    }

    if bytes[index] == b'"' {
      index += 1;
      let mut value = String::new();
      let mut closed = false;

      while index < bytes.len() {
        match bytes[index] {
          b'"' => {
            index += 1;
            closed = true;
            break;
          },
          b'\\' => {
            let escaped = *bytes
              .get(index + 1)
              .ok_or_else(|| invalid("a quoted Exec argument ends with an incomplete escape"))?;
            if !matches!(escaped, b'"' | b'`' | b'$' | b'\\') {
              return Err(invalid(format!(
                "invalid escape \\{} in a quoted Exec argument",
                escaped as char
              )));
            }
            value.push(escaped as char);
            index += 2;
          },
          b'$' | b'`' => {
            return Err(invalid(format!(
              "reserved character {:?} must be escaped in a quoted Exec argument",
              bytes[index] as char
            )));
          },
          byte => {
            value.push(byte as char);
            index += 1;
          },
        }
      }

      if !closed {
        return Err(invalid("unmatched double quote in Exec"));
      }
      if index < bytes.len() && bytes[index] != b' ' {
        return Err(invalid("an Exec argument must be quoted in whole"));
      }

      arguments.push(Argument { value, quoted: true });
      continue;
    }

    let start = index;
    while index < bytes.len() && bytes[index] != b' ' {
      if is_reserved(bytes[index]) {
        return Err(invalid(format!(
          "reserved character {:?} must occur inside a quoted Exec argument",
          bytes[index] as char
        )));
      }
      index += 1;
    }

    arguments.push(Argument {
      value: exec[start..index].to_string(),
      quoted: false,
    });
  }

  Ok(arguments)
}

fn expand_quoted_percent(value: &str) -> io::Result<String> {
  let bytes = value.as_bytes();
  let mut output = String::with_capacity(value.len());
  let mut index = 0;

  while index < bytes.len() {
    if bytes[index] != b'%' {
      output.push(bytes[index] as char);
      index += 1;
      continue;
    }

    match bytes.get(index + 1) {
      Some(b'%') => {
        output.push('%');
        index += 2;
      },
      Some(code) if code.is_ascii_alphabetic() => {
        return Err(invalid("Exec field codes must not occur inside quoted arguments"));
      },
      _ => return Err(invalid("a literal percent sign in Exec must be written as %%")),
    }
  }

  Ok(output)
}

fn mark_file_code(seen: &mut bool) -> io::Result<()> {
  if *seen {
    return Err(invalid("Exec may contain at most one of %f, %F, %u, or %U"));
  }
  *seen = true;
  Ok(())
}

fn is_reserved(byte: u8) -> bool {
  matches!(
    byte,
    b'\t'
      | b'\n'
      | b'"'
      | b'\''
      | b'\\'
      | b'>'
      | b'<'
      | b'~'
      | b'|'
      | b'&'
      | b';'
      | b'$'
      | b'*'
      | b'?'
      | b'#'
      | b'('
      | b')'
      | b'`'
  )
}

fn invalid(message: impl Into<String>) -> io::Error {
  io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
  use std::{path::Path, process::Command};

  use super::{parse_exec, shell_join};

  #[test]
  fn parses_exec_quoting_and_escaping() {
    let arguments = parse_exec(
      r#"runner "two words" "quote\"tick\`dollar\$slash\\" plain"#,
      "Session",
      None,
      Path::new("/session.desktop"),
    )
    .unwrap();

    assert_eq!(arguments, [
      "runner",
      "two words",
      "quote\"tick`dollar$slash\\",
      "plain"
    ]);
  }

  #[test]
  fn expands_and_removes_supported_field_codes() {
    let arguments = parse_exec(
      "runner %% before%cafter %i %f %d %k",
      "My Session",
      Some("my icon"),
      Path::new("/tmp/a b.desktop"),
    )
    .unwrap();

    assert_eq!(arguments, [
      "runner",
      "%",
      "beforeMy Sessionafter",
      "--icon",
      "my icon",
      "/tmp/a b.desktop"
    ]);
  }

  #[test]
  fn rejects_invalid_exec_forms() {
    for exec in [
      "",
      "\"\" argument",
      "runner \"unterminated",
      "runner \"quoted\"tail",
      "runner 'single'",
      "runner \"raw$dollar\"",
      "runner \"%c\"",
      "runner %Ftail",
      "runner %f %U",
      "runner %x",
      "runner %",
      "run=ner",
      "runnér",
    ] {
      assert!(
        parse_exec(exec, "Session", None, Path::new("/session.desktop")).is_err(),
        "accepted invalid Exec value {exec:?}"
      );
    }
  }

  #[test]
  fn shell_join_preserves_every_argument() {
    let arguments = ["runner", "two words", "a'b", "$HOME; true", ""].map(str::to_string);
    let command = shell_join(&arguments);

    assert_eq!(command, "'runner' 'two words' 'a'\\''b' '$HOME; true' ''");

    let output = Command::new("/bin/sh")
      .args(["-c", &format!("set -- {command}; printf '<%s>\\n' \"$@\"")])
      .output()
      .unwrap();
    assert!(output.status.success());
    assert_eq!(
      String::from_utf8(output.stdout).unwrap(),
      "<runner>\n<two words>\n<a'b>\n<$HOME; true>\n<>\n"
    );
  }
}

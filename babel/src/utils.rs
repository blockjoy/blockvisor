use crate::{error, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, BufReader};

pub async fn file_checksum(path: &PathBuf) -> Result<u32> {
    let file = File::open(path).await?;
    let mut reader = BufReader::new(file);
    let mut buf = [0; 16384];
    let crc = crc::Crc::<u32>::new(&crc::CRC_32_BZIP2);
    let mut digest = crc.digest();
    while let Ok(size) = reader.read(&mut buf[..]).await {
        if size == 0 {
            break;
        }
        digest.update(&buf[0..size]);
    }
    Ok(digest.finalize())
}

/// Renders a template by filling in uppercased, `{{ }}`-delimited template strings with the
/// values in the `params` dictionary.
pub fn render(template: &str, params: &HashMap<String, String>) -> String {
    let mut res = template.to_string();
    for (k, v) in params {
        // This formats a parameter like `url` as `{{URL}}`
        let k = format!("{{{{{}}}}}", k.to_uppercase());
        res = res.replace(&k, v);
    }
    res
}

/// Allowing people to substitute arbitrary data into sh-commands is unsafe. We therefore run
/// this function over each value before we substitute it. This function is deliberately more
/// restrictive than needed; it just filters out each character that is not a number or a
/// string or absolutely needed to form a url or json file.
pub fn sanitize_param(param: &[String]) -> Result<String> {
    let res = param
        .iter()
        // We escape each individual argument
        .map(|p| p.chars().map(escape_char).collect::<Result<String>>())
        // Now join the iterator of Strings into a single String, using `" "` as a seperator.
        // This means our final string looks like `"arg 1" "arg 2" "arg 3"`, and that makes it
        // ready to be subsituted into the sh command.
        .try_fold("\"".to_string(), |acc, elem| {
            elem.map(|elem| acc + "\" \"" + &elem)
        })?;
    Ok(res + "\"")
}

/// If the character is allowed, escapes a character into something we can use for a
/// bash-substitution.
pub fn escape_char(c: char) -> Result<String> {
    match c {
        // Alphanumerics do not need escaping.
        _ if c.is_alphanumeric() => Ok(c.to_string()),
        // Quotes need to be escaped.
        '"' => Ok("\\\"".to_string()),
        // Newlines must be esacped
        '\n' => Ok("\\n".to_string()),
        // These are the special characters we allow that do not need esacping.
        '/' | ':' | '{' | '}' | ' ' => Ok(c.to_string()),
        // If none of these cases match, we return an error.
        _ => Err(error::Error::unsafe_sub()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_render() {
        let s = |s: &str| s.to_string(); // to make the test less verbose
        let par1 = s("val1");
        let par2 = s("val2");
        let par3 = s("val3 val4");
        let params = [(s("par1"), par1), (s("pAr2"), par2), (s("PAR3"), par3)]
            .into_iter()
            .collect();
        let render = |template| render(template, &params);

        assert_eq!(render("{{PAR1}} bla"), "val1 bla");
        assert_eq!(render("{{PAR2}} waa"), "val2 waa");
        assert_eq!(render("{{PAR3}} kra"), "val3 val4 kra");
        assert_eq!(render("{{par1}} woo"), "{{par1}} woo");
        assert_eq!(render("{{pAr2}} koo"), "{{pAr2}} koo");
        assert_eq!(render("{{PAR3}} doo"), "val3 val4 doo");
    }
}

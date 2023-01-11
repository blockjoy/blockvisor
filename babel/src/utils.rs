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
        .try_fold("".to_string(), |acc, elem| {
            elem.map(|elem| acc + " \"" + &elem + "\"")
        })?;
    Ok(res)
}

/// If the character is allowed, escapes a character into something we can use for a
/// bash-substitution.
fn escape_char(c: char) -> Result<String> {
    match c {
        // Alphanumerics do not need escaping.
        _ if c.is_alphanumeric() => Ok(c.to_string()),
        // Quotes need to be escaped.
        '"' => Ok("\\\"".to_string()),
        // Newlines must be esacped
        '\n' => Ok("\\n".to_string()),
        // These are the special characters we allow that do not need esacping.
        '/' | ':' | '{' | '}' | ',' | '-' | ' ' => Ok(c.to_string()),
        // If none of these cases match, we return an error.
        c => Err(error::Error::unsafe_sub(c)),
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

    #[test]
    fn test_sanitize_param() {
        let params1 = [
            "some".to_string(),
            "test".to_string(),
            "strings".to_string(),
        ];
        let sanitized1 = sanitize_param(&params1).unwrap();
        assert_eq!(sanitized1, r#" "some" "test" "strings""#);

        let params2 = [
            "some\n".to_string(),
            "test/".to_string(),
            "strings\"".to_string(),
        ];
        let sanitized2 = sanitize_param(&params2).unwrap();
        assert_eq!(sanitized2, r#" "some\n" "test/" "strings\"""#);

        sanitize_param(&[r#"{"crypto":{"kdf":{"function":"scrypt","params":{"dklen":32,"n":262144,"r":8,"p":1,"salt":"f36fe9215c3576941742cd295935f678df4d2b3697b62c0f52b43b21b540d2d0"},"message":""},"checksum":{"function":"sha256","params":{},"message":"a686c26f070ebdcd848d6445685a287d9ba557acdf94551ad9199fe3f4335ca9"},"cipher":{"function":"aes-128-ctr","params":{"iv":"e41ee5ea6099bb2b98d4dad8d08301b3"},"message":"37f6ab34a7e484a5b1cf9907d6464b8f89852f3914baff93f1dd2fcf54352986"}},"description":"","pubkey":"a7d3b17b67320381d10fa111c71eee89a728f36d8fbfcd294807fe8b8d27d6a95ee5cdc0bf05d6b2a4f9ac08699747e9","path":"m/12381/3600/0/0/0","uuid":"2f89ee56-b65a-4142-9df0-abb42addccd4","version":4}"#.to_string()]).unwrap();
    }
}

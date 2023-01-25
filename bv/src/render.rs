//! This file contains the functions that we use for rendering (mostly sh) commands that are sent to
//! the nodes.

use anyhow::{anyhow, bail, Result};
use std::collections::HashMap;
use std::fmt::Display;

use crate::utils;

/// Two phase renderer that first takes template parameters specified by the user and applies them,
/// and then takes the template parameters specified by the babel config and applies them. These
/// two passes are split out into two 'phases' and are ran successively.
pub fn render(
    template: &str,
    params: &HashMap<impl Display, impl Display>,
    config: &toml::Value,
) -> Result<String> {
    let template = render_params(template, params)?;
    render_config(&template, config)
}

/// Phase 1 of the rendering process entails taking the parameters that were specified by the user,
/// uppercasing and `{{ }}`-delimiting them, and replacing those values in the template.
fn render_params(template: &str, params: &HashMap<impl Display, impl Display>) -> Result<String> {
    let mut res = template.to_string();
    for (key, value) in params {
        // This formats a parameter like `url` as `{{URL}}`
        let placeholder = format!("{{{{{}}}}}", key.to_string().to_uppercase());
        res = res.replace(&placeholder, &value.to_string());
    }
    fn find_placeholder(value: &str) -> Option<&str> {
        let start_idx = value.find("{{")?;
        let end_idx = start_idx + value[start_idx..].find("}}")? + 2;
        Some(&value[start_idx..end_idx])
    }
    if let Some(placeholder) = find_placeholder(&res) {
        bail!("parameter {placeholder} is missing")
    }
    Ok(res)
}

/// For phase 2 of our two step renderer we will go looking for "babel_refs" in the template that
/// are of the form `babelref:'some.dot.delimited.path'`. These are then replaced with the values
/// as specfied in `config`. For example, if `config` looks like this:
/// ```rs
/// let config = toml::toml!(
/// [network]
/// ip = "192.168.1.10^100"
/// );
/// ```
/// Then a template like `curl babelref:'network.ip'` will be rendered as `curl 192.168.1.10^100`.
fn render_config(template: &str, config: &toml::Value) -> Result<String> {
    const DELIM_START: &str = "babelref:'";
    const DELIM_END: &str = "'";

    fn next_babel_ref(template: &str) -> Option<(usize, &str)> {
        let start_idx = template.find(DELIM_START)? + DELIM_START.len();
        let end_idx = start_idx + template[start_idx..].find(DELIM_END)?;
        Some((end_idx, &template[start_idx..end_idx]))
    }

    // First we find the "babel_refs" that are in the template.
    let mut babel_refs = vec![];
    let mut cur = 0;
    while let Some((offset, babel_ref)) = next_babel_ref(&template[cur..]) {
        babel_refs.push(babel_ref);
        cur += offset;
    }
    // Now we replace each babel_ref in template with the value retrieved from `config`.
    let mut rendered = template.to_string();
    for elem in babel_refs {
        let babel_ref = format!("{DELIM_START}{elem}{DELIM_END}");
        let value = utils::get_config_value_by_path(config, elem)
            .ok_or_else(|| anyhow!("referenced value {babel_ref} not found in babel"))?;
        rendered = rendered.replace(&babel_ref, &value)
    }
    Ok(rendered)
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
        // This means our final string looks like `" arg 1" "arg 2" "arg 3"`, and that makes it
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
        // Explicit disallowance of ', since that is the delimiter we use in `render_config`.
        '\'' => anyhow::bail!("Very unsafe subsitution >:( {c}"),
        // Alphanumerics do not need escaping.
        _ if c.is_alphanumeric() => Ok(c.to_string()),
        // Quotes need to be escaped.
        '"' => Ok("\\\"".to_string()),
        // Newlines must be esacped
        '\n' => Ok("\\n".to_string()),
        // These are the special characters we allow that do not need esacping.
        '/' | ':' | '{' | '}' | ',' | '-' | ' ' => Ok(c.to_string()),
        // If none of these cases match, we return an error.
        c => anyhow::bail!("Unsafe subsitution: {c}"),
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    #[test]
    fn test_render_params() -> Result<()> {
        let s = |s: &str| s.to_string(); // to make the test less verbose
        let par1 = s("val1");
        let par2 = s("val2");
        let par3 = s("val3 val4");
        let params = [(s("par1"), par1), (s("pAr2"), par2), (s("PAR3"), par3)]
            .into_iter()
            .collect();
        let render = |template| render_params(template, &params);

        assert_eq!(render("{{PAR1}} bla")?, "val1 bla");
        assert_eq!(render("{{PAR2}} waa")?, "val2 waa");
        assert_eq!(render("{{PAR3}} kra")?, "val3 val4 kra");
        render("{{par1}} woo").unwrap_err();
        render("{{pAr2}} koo").unwrap_err();
        assert_eq!(render("{{PAR3}} doo")?, "val3 val4 doo");
        Ok(())
    }

    #[test]
    fn test_render_config() -> Result<()> {
        let template = "curl \"babelref:'some.seg.ment.list'\"";
        let val: toml::Value = toml::toml!(
        [some]
        [some.seg]
        [some.seg.ment]
        list = "all the way down here."
        );
        assert_eq!(
            "curl \"all the way down here.\"",
            render_config(template, &val)?,
        );
        Ok(())
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

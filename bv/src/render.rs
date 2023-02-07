//! This file contains the functions that we use for rendering (mostly sh) commands that are sent to
//! the nodes.

use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;

use crate::utils;

/// Two phase rendering that first resolve babelrefs an then takes template parameters specified and applies them.
pub fn render_with(
    template: &str,
    params: &HashMap<String, Vec<String>>,
    config: &toml::Value,
    render_param: impl FnMut(&Vec<String>) -> Result<String>,
) -> Result<String> {
    let template = resolve_babelrefs(template, config, params)?;
    render_params(&template, params, render_param)
}

/// Replace all placeholders enclosed by `{{ }}` with value that were specified in `params`,
/// first applying `render_param` function on it.
/// Parameter names are case insensitive.
fn render_params(
    template: &str,
    params: &HashMap<String, Vec<String>>,
    mut render_param: impl FnMut(&Vec<String>) -> Result<String>,
) -> Result<String> {
    const DELIM_START: &str = "{{";
    const DELIM_END: &str = "}}";
    // First we find the placeholders that are in the template.
    let placeholders = find_unique_placeholders(template, DELIM_START, DELIM_END);
    let mut rendered = template.to_string();
    for key in placeholders {
        if let Some(value) = params
            .iter()
            .find(|(k, _)| k.to_uppercase() == key.to_uppercase())
            .map(|(_, v)| v)
        {
            let placeholder = format!("{DELIM_START}{key}{DELIM_END}");
            rendered = rendered.replace(
                &placeholder,
                &render_param(value)
                    .with_context(|| format!("Parameter {key} has invalid value: {value:?}"))?,
            );
        } else {
            bail!("parameter {key} is missing")
        }
    }
    Ok(rendered)
}

/// Resolve babel.toml internal references.
/// First looking for "babel_refs" in the template that are of the form
/// `babelref:'some.dot.delimited.{{MAYBE_PARAMETRIZED}}.path'`.
/// Then it validate and render parameters (if any) according to provided `params`.
/// Finally these are then replaced with the values as specfied in `config`.
/// For example, if `config` looks like this:
/// ```rs
/// let config = toml::toml!(
/// [network]
/// ip = "192.168.1.10^100"
/// );
/// ```
/// Then a template like `curl babelref:'network.ip'` will be rendered as `curl 192.168.1.10^100`.
fn resolve_babelrefs(
    template: &str,
    config: &toml::Value,
    params: &HashMap<String, Vec<String>>,
) -> Result<String> {
    const DELIM_START: &str = "babelref:'";
    const DELIM_END: &str = "'";
    // First we find the "babel_refs" that are in the template.
    let babel_refs = find_unique_placeholders(template, DELIM_START, DELIM_END);
    // Now we replace each babel_ref in template with the value retrieved from `config`.
    let mut rendered = template.to_string();
    for babel_ref in babel_refs {
        let placeholder = format!("{DELIM_START}{babel_ref}{DELIM_END}");
        let babel_ref = render_params(babel_ref, params, render_babelref_param)
            .with_context(|| "failed to render babelref parameter")?;
        let value = utils::get_config_value_by_path(config, &babel_ref)
            .ok_or_else(|| anyhow!("referenced value {babel_ref} not found in babel"))?;
        rendered = rendered.replace(&placeholder, &value)
    }
    Ok(rendered)
}

/// Find all placeholders enclosed by `delim_start` and `delim_end` in given `template`.
fn find_unique_placeholders<'a>(
    template: &'a str,
    delim_start: &str,
    delim_end: &str,
) -> Vec<&'a str> {
    let next_placeholder = |template: &'a str| -> Option<(usize, &'a str)> {
        let start_idx = template.find(delim_start)? + delim_start.len();
        let end_idx = start_idx + template[start_idx..].find(delim_end)?;
        Some((end_idx, &template[start_idx..end_idx]))
    };

    let mut placeholders = vec![];
    let mut cur = 0;
    while let Some((offset, placeholder)) = next_placeholder(&template[cur..]) {
        placeholders.push(placeholder);
        cur += offset;
    }
    placeholders.sort();
    placeholders.dedup();
    placeholders
}

/// Allowing people to substitute arbitrary data into sh-commands is unsafe. We therefore run
/// this function over each value before we substitute it. This function is deliberately more
/// restrictive than needed; it just filters out each character that is not a number or a
/// string or absolutely needed to form a url or json file.
pub fn render_sh_param(param: &Vec<String>) -> Result<String> {
    let mut items = Vec::default();
    for item in param {
        // We escape each individual argument
        items.push(format!(
            "\"{}\"",
            item.chars()
                .map(escape_sh_char)
                .collect::<Result<String>>()?
        ));
    }
    // Now join the iterator of Strings into a single String, using `" "` as a seperator.
    // This means our final string looks like `"arg 1" "arg 2" "arg 3"`, and that makes it
    // ready to be subsituted into the sh command.
    Ok(items.join(" "))
}

/// If the character is allowed, escapes a character into something we can use for a
/// bash-substitution.
fn escape_sh_char(c: char) -> Result<String> {
    match c {
        // Explicit disallowance of ', since that is the delimiter we use in `render_config`.
        '\'' => bail!("Very unsafe subsitution >:( {c}"),
        // Alphanumerics do not need escaping.
        _ if c.is_alphanumeric() => Ok(c.to_string()),
        // Quotes need to be escaped.
        '"' => Ok("\\\"".to_string()),
        // Newlines must be esacped
        '\n' => Ok("\\n".to_string()),
        // These are the special characters we allow that do not need esacping.
        '/' | ':' | '{' | '}' | ',' | '-' | '_' | '.' | ' ' => Ok(c.to_string()),
        // If none of these cases match, we return an error.
        c => bail!("Shell unsafe character detected: {c}"),
    }
}

fn render_babelref_param(value: &Vec<String>) -> Result<String> {
    if value.len() > 1 {
        bail!("multi value not allowed in babelref")
    }
    match value.first() {
        Some(value)
            if value
                .chars()
                .all(|c| c.is_alphanumeric() || "_-.".contains(c)) =>
        {
            Ok(value.clone())
        }
        _ => bail!("parameter value not allowed: '{value:?}"),
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// to make the test less verbose
    fn s(s: &str) -> String {
        s.to_string()
    }

    #[test]
    fn test_render_params() -> Result<()> {
        let par1 = s("val1");
        let par2 = s("val2");
        let par3a = s("val3 val4");
        let par3b = s("val5");
        let params = [
            (s("par1"), vec![par1]),
            (s("pAr2"), vec![par2]),
            (s("PAR3"), vec![par3a, par3b]),
        ]
        .into_iter()
        .collect();
        let render = |template| render_params(template, &params, |v| Ok(v.join(".")));

        assert_eq!(render("{{PAR1}} bla")?, "val1 bla");
        assert_eq!(render("{{PAR2}} waa")?, "val2 waa");
        assert_eq!(render("{{PAR3}} kra")?, "val3 val4.val5 kra");
        assert_eq!(render("{{par1}} bla")?, "val1 bla");
        assert_eq!(render("{{pAr2}} waa")?, "val2 waa");
        assert_eq!(render("{{PAR3}} doo")?, "val3 val4.val5 doo");
        render("{{missing}} doo").unwrap_err();
        Ok(())
    }

    #[test]
    fn test_render_config() -> Result<()> {
        let params = [
            (s("par1"), vec![s("seg.ment")]),
            (s("par2"), vec![s("in/valid")]),
            (s("par3"), vec![]),
            (s("PAR4"), vec![s("to"), s("many")]),
        ]
        .into_iter()
        .collect();

        let val: toml::Value = toml::toml!(
        [some]
        [some.seg]
        [some.seg.ment]
        list = "all the way down here."
        );
        assert_eq!(
            "curl \"all the way down here.\"",
            resolve_babelrefs("curl \"babelref:'some.{{PAR1}}.list'\"", &val, &params)?,
        );
        resolve_babelrefs("curl \"babelref:'some.{{PAR2}}.ment.list'\"", &val, &params)
            .unwrap_err();
        resolve_babelrefs("curl \"babelref:'some.{{PAR3}}.ment.list'\"", &val, &params)
            .unwrap_err();
        resolve_babelrefs("curl \"babelref:'some.{{PAR4}}.ment.list'\"", &val, &params)
            .unwrap_err();
        Ok(())
    }

    #[test]
    fn test_sanitize_param() {
        let params1 = vec![s("some"), s("test"), s("strings")];
        let sanitized1 = render_sh_param(&params1).unwrap();
        assert_eq!(sanitized1, r#""some" "test" "strings""#);

        let params2 = vec![s("some\n"), s("test/"), s("strings\"")];
        let sanitized2 = render_sh_param(&params2).unwrap();
        assert_eq!(sanitized2, r#""some\n" "test/" "strings\"""#);

        let params3 = vec![s("single param")];
        let sanitized3 = render_sh_param(&params3).unwrap();
        assert_eq!(sanitized3, r#""single param""#);

        render_sh_param(&vec![s(r#"{"crypto":{"kdf":{"function":"scrypt","params":{"dklen":32,"n":262144,"r":8,"p":1,"salt":"f36fe9215c3576941742cd295935f678df4d2b3697b62c0f52b43b21b540d2d0"},"message":""},"checksum":{"function":"sha256","params":{},"message":"a686c26f070ebdcd848d6445685a287d9ba557acdf94551ad9199fe3f4335ca9"},"cipher":{"function":"aes-128-ctr","params":{"iv":"e41ee5ea6099bb2b98d4dad8d08301b3"},"message":"37f6ab34a7e484a5b1cf9907d6464b8f89852f3914baff93f1dd2fcf54352986"}},"description":"","pubkey":"a7d3b17b67320381d10fa111c71eee89a728f36d8fbfcd294807fe8b8d27d6a95ee5cdc0bf05d6b2a4f9ac08699747e9","path":"m/12381/3600/0/0/0","uuid":"2f89ee56-b65a-4142-9df0-abb42addccd4","version":4}"#)]).unwrap();
    }
}

use babel_api::config::firewall::{Config, Protocol, Rule};
use eyre::{bail, Result};
use serde_variant::to_variant_name;

pub async fn apply_firewall_config(config: Config) -> Result<()> {
    if config.enabled {
        // first convert config to convenient structure
        let rule_args = RuleArgs::from_rules(&config.rules);
        // dry-run rules to make sure they are valid
        for args in &rule_args {
            dry_run_ufw(&args.into()).await?;
        }
        //finally reset and apply whole firewall config
        run_ufw(&["reset", "--force"]).await?;
        run_ufw(&["enable"]).await?;
        run_ufw(&["default", variant_to_string(&config.default_in), "incoming"]).await?;
        run_ufw(&[
            "default",
            variant_to_string(&config.default_out),
            "outgoing",
        ])
        .await?;
        // and actually apply rules
        for args in &rule_args {
            run_ufw(&args.into()).await?;
        }
    } else {
        run_ufw(&["disable"]).await?;
    }
    Ok(())
}

pub struct RuleArgs<'a> {
    policy: &'a str,
    direction: &'a str,
    protocol: &'a str,
    ips: &'a str,
    port: String,
    name: &'a str,
}

impl<'a> RuleArgs<'a> {
    pub fn from_rules(rules: &'a Vec<Rule>) -> Vec<Self> {
        let mut rule_args = Vec::default();
        for rule in rules {
            let proto = rule.protocol.as_ref();
            for port in &rule.ports {
                rule_args.push(Self {
                    policy: variant_to_string(&rule.policy),
                    direction: variant_to_string(&rule.direction),
                    protocol: variant_to_string(proto.unwrap_or(&Protocol::Both)),
                    ips: rule.ips.as_ref().map_or("any", |ip| ip.as_str()),
                    port: port.to_string(),
                    name: rule.name.as_str(),
                });
            }
        }
        rule_args
    }

    pub fn into(&self) -> [&str; 10] {
        [
            self.policy,
            self.direction,
            "proto",
            self.protocol,
            "from",
            self.ips,
            "port",
            self.port.as_str(),
            "comment",
            self.name,
        ]
    }
}

pub fn variant_to_string<T: serde::ser::Serialize>(variant: &T) -> &str {
    // `to_variant_name()` may fail only with `UnsupportedType` which shall not happen,
    // so it is safe to unwrap here.
    to_variant_name(variant).unwrap()
}

pub async fn dry_run_ufw(args: &[&str]) -> Result<()> {
    run_ufw(&[&["--dry-run"], args].concat()).await
}

pub async fn run_ufw(args: &[&str]) -> Result<()> {
    let output = tokio::process::Command::new("ufw")
        .args(args)
        .output()
        .await?;
    if !output.status.success() {
        bail!("Failed to run command 'ufw', got output `{output:?}`");
    }
    Ok(())
}

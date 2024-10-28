use crate::pal::NodeFirewallConfig;
use async_trait::async_trait;
use babel_api::metadata::firewall::{Direction, Protocol};
use eyre::{bail, Result};
use serde_variant::to_variant_name;
use uuid::Uuid;

pub async fn apply_firewall_config(config: NodeFirewallConfig) -> Result<()> {
    apply_firewall_config_with(config, SysRunner).await
}

pub async fn cleanup_node_rules(node_id: Uuid) -> Result<()> {
    cleanup_node_rules_with(node_id, &SysRunner).await
}

#[async_trait]
trait UfwRunner {
    async fn run<'a>(&self, args: &[&'a str]) -> Result<String>;
}

struct SysRunner;

#[async_trait]
impl UfwRunner for SysRunner {
    async fn run<'a>(&self, args: &[&'a str]) -> Result<String> {
        let output = tokio::process::Command::new("ufw")
            .args(args)
            .output()
            .await?;
        if !output.status.success() {
            let args_str = args.join(" ");
            bail!("Failed to run command 'ufw {args_str}', got output: `{output:?}`");
        } else {
            Ok(String::from_utf8(output.stdout)?)
        }
    }
}

async fn apply_firewall_config_with(
    config: NodeFirewallConfig,
    runner: impl UfwRunner,
) -> Result<()> {
    let id = config.id;
    let iface = config.iface.clone();
    // first convert config to convenient structure
    let rule_args = RuleArgs::from_rules(config);
    // dry-run rules to make sure they are valid
    for args in &rule_args {
        dry_run(&runner, &args.as_vec(&iface)).await?;
    }
    //clean old node rules
    cleanup_node_rules_with(id, &runner).await?;
    // and apply new rules
    for args in &rule_args {
        runner.run(&args.as_vec(&iface)).await?;
    }
    Ok(())
}

async fn cleanup_node_rules_with(node_id: Uuid, runner: &impl UfwRunner) -> Result<()> {
    let stdout = runner.run(&["status", "numbered"]).await?;
    let mut rules = stdout
        .lines()
        .filter_map(|line| {
            if line.contains(&node_id.to_string()) {
                line.split_once(']').and_then(|(number, _)| {
                    number.trim_start_matches('[').trim().parse::<usize>().ok()
                })
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    rules.sort();
    while let Some(rule) = rules.pop() {
        runner
            .run(&["--force", "delete", &format!("{rule}")])
            .await?;
    }
    Ok(())
}

struct RuleArgs {
    policy: String,
    direction: Direction,
    protocol: Option<String>,
    ip_from: String,
    ip_to: String,
    port: Option<String>, // no port means - no port argument passed at all i.e. rule apply for all ports
    name: String,
}

impl RuleArgs {
    fn from_rules(config: NodeFirewallConfig) -> Vec<Self> {
        let mut rule_args = Vec::default();
        for rule in config.config.rules.into_iter().rev() {
            let proto = rule.protocol.as_ref().and_then(|proto| {
                if &Protocol::Both != proto {
                    Some(variant_to_string(proto))
                } else {
                    None
                }
            });
            let ips = rule.ips.map_or("any".to_string(), |ip| ip);
            let name = format!("{} {}", config.id, rule.name);
            let (ip_from, ip_to) = match rule.direction {
                Direction::Out => (config.ip.to_string(), ips),
                Direction::In => (ips, config.ip.to_string()),
            };

            if rule.ports.is_empty() {
                rule_args.push(Self {
                    policy: variant_to_string(&rule.action),
                    direction: rule.direction,
                    protocol: proto,
                    ip_from,
                    ip_to,
                    port: None,
                    name,
                });
            } else {
                for port in &rule.ports {
                    rule_args.push(Self {
                        policy: variant_to_string(&rule.action),
                        direction: rule.direction.clone(),
                        protocol: proto.clone(),
                        ip_from: ip_from.clone(),
                        ip_to: ip_to.clone(),
                        port: Some(port.to_string()),
                        name: name.clone(),
                    });
                }
            }
        }
        rule_args.push(Self {
            policy: variant_to_string(&config.config.default_in),
            direction: Direction::In,
            protocol: None,
            ip_from: "any".to_string(),
            ip_to: config.ip.to_string(),
            port: None,
            name: format!("{} default in", config.id),
        });
        rule_args.push(Self {
            policy: variant_to_string(&config.config.default_out),
            direction: Direction::Out,
            protocol: None,
            ip_from: config.ip.to_string(),
            ip_to: "any".to_string(),
            port: None,
            name: format!("{} default out", config.id),
        });
        rule_args
    }

    fn as_vec<'a>(&'a self, iface: &'a str) -> Vec<&'a str> {
        let mut args = vec![
            "route",
            self.policy.as_str(),
            to_variant_name(&self.direction).unwrap(),
            "on",
            iface,
            "from",
            &self.ip_from,
            "to",
            &self.ip_to,
        ];
        if let Some(port) = &self.port {
            args.push("port");
            args.push(port.as_str());
        }
        if let Some(proto) = &self.protocol {
            args.push("proto");
            args.push(proto.as_str());
        }
        args.push("comment");
        args.push(&self.name);
        args
    }
}

fn variant_to_string<T: serde::ser::Serialize>(variant: &T) -> String {
    // `to_variant_name()` may fail only with `UnsupportedType` which shall not happen,
    // so it is safe to unwrap here.
    to_variant_name(variant).unwrap().to_string()
}

async fn dry_run(runner: &impl UfwRunner, args: &[&str]) -> Result<String> {
    runner.run(&[&["--dry-run"], args].concat()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use babel_api::metadata::firewall;
    use babel_api::metadata::firewall::{Action, Rule};
    use mockall::*;
    use std::net::IpAddr;
    use std::str::FromStr;

    mock! {
        pub TestRunner {}

        #[async_trait]
        impl UfwRunner for TestRunner {
            async fn run<'a>(&self, args: &[&'a str]) -> Result<String>;
        }
    }

    fn expect_with_args(mock_runner: &mut MockTestRunner, expected_args: &'static [&str]) {
        mock_runner
            .expect_run()
            .once()
            .withf(move |args| args == expected_args)
            .returning(|_| Ok(String::default()));
    }

    fn default_config() -> NodeFirewallConfig {
        NodeFirewallConfig {
            id: Uuid::parse_str("4931bafa-92d9-4521-9fc6-a77eee047530").unwrap(),
            ip: IpAddr::from_str("192.168.0.7").unwrap(),
            iface: "bvbr0".to_string(),
            config: firewall::Config {
                default_in: Action::Deny,
                default_out: Action::Allow,
                rules: vec![],
            },
        }
    }

    #[tokio::test]
    async fn test_run_failed() -> Result<()> {
        let config = default_config();
        let mut mock_runner = MockTestRunner::new();
        mock_runner
            .expect_run()
            .once()
            .withf(|args| {
                args == [
                    "--dry-run",
                    "route",
                    "deny",
                    "in",
                    "on",
                    "bvbr0",
                    "from",
                    "any",
                    "to",
                    "192.168.0.7",
                    "comment",
                    "4931bafa-92d9-4521-9fc6-a77eee047530 default in",
                ]
            })
            .returning(|_| bail!("test_error"));

        assert_eq!(
            "test_error",
            apply_firewall_config_with(config, mock_runner)
                .await
                .unwrap_err()
                .to_string()
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_no_rules() -> Result<()> {
        let config = default_config();
        let mut mock_runner = MockTestRunner::new();
        expect_with_args(
            &mut mock_runner,
            &[
                "--dry-run",
                "route",
                "deny",
                "in",
                "on",
                "bvbr0",
                "from",
                "any",
                "to",
                "192.168.0.7",
                "comment",
                "4931bafa-92d9-4521-9fc6-a77eee047530 default in",
            ],
        );
        expect_with_args(
            &mut mock_runner,
            &[
                "--dry-run",
                "route",
                "allow",
                "out",
                "on",
                "bvbr0",
                "from",
                "192.168.0.7",
                "to",
                "any",
                "comment",
                "4931bafa-92d9-4521-9fc6-a77eee047530 default out",
            ],
        );
        mock_runner
            .expect_run()
            .once()
            .withf(|args| args == ["status", "numbered"])
            .returning(|_| Ok("anything\n\
            [ 1] 192.168.0.7               ALLOW IN    Anywhere                   # 4931bafa-92d9-4521-9fc6-a77eee047530: name\n
            [ 3 ] 192.168.0.13 8000          ALLOW IN    Anywhere                   # 5931bafa-92d9-4521-9fc6-a77eee047530: another name\n\
            [ 7 ] 192.168.0.7 8000          ALLOW IN    Anywhere                   # 4931bafa-92d9-4521-9fc6-a77eee047530: another name".to_string()));
        expect_with_args(&mut mock_runner, &["--force", "delete", "1"]);
        expect_with_args(&mut mock_runner, &["--force", "delete", "7"]);
        expect_with_args(
            &mut mock_runner,
            &[
                "route",
                "deny",
                "in",
                "on",
                "bvbr0",
                "from",
                "any",
                "to",
                "192.168.0.7",
                "comment",
                "4931bafa-92d9-4521-9fc6-a77eee047530 default in",
            ],
        );
        expect_with_args(
            &mut mock_runner,
            &[
                "route",
                "allow",
                "out",
                "on",
                "bvbr0",
                "from",
                "192.168.0.7",
                "to",
                "any",
                "comment",
                "4931bafa-92d9-4521-9fc6-a77eee047530 default out",
            ],
        );

        apply_firewall_config_with(config, mock_runner).await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_with_rules() -> Result<()> {
        let mut config = default_config();
        config.config.rules = vec![
            Rule {
                name: "rule A".to_string(),
                action: Action::Allow,
                direction: Direction::Out,
                protocol: None,
                ips: Some("ip.is.validated.before".to_string()),
                ports: vec![7],
            },
            Rule {
                name: "rule B".to_string(),
                action: Action::Allow,
                direction: Direction::In,
                protocol: Some(Protocol::Tcp),
                ips: Some("ip.is.validated.before".to_string()),
                ports: vec![144, 77],
            },
            Rule {
                name: "no ports".to_string(),
                action: Action::Allow,
                direction: Direction::Out,
                protocol: None,
                ips: None,
                ports: vec![],
            },
            Rule {
                name: "".to_string(),
                action: Action::Allow,
                direction: Direction::Out,
                protocol: None,
                ips: None,
                ports: vec![7],
            },
        ];
        let mut mock_runner = MockTestRunner::new();
        expect_with_args(
            &mut mock_runner,
            &[
                "--dry-run",
                "route",
                "allow",
                "out",
                "on",
                "bvbr0",
                "from",
                "192.168.0.7",
                "to",
                "any",
                "port",
                "7",
                "comment",
                "4931bafa-92d9-4521-9fc6-a77eee047530 ",
            ],
        );
        expect_with_args(
            &mut mock_runner,
            &[
                "--dry-run",
                "route",
                "allow",
                "out",
                "on",
                "bvbr0",
                "from",
                "192.168.0.7",
                "to",
                "any",
                "comment",
                "4931bafa-92d9-4521-9fc6-a77eee047530 no ports",
            ],
        );
        expect_with_args(
            &mut mock_runner,
            &[
                "--dry-run",
                "route",
                "allow",
                "in",
                "on",
                "bvbr0",
                "from",
                "ip.is.validated.before",
                "to",
                "192.168.0.7",
                "port",
                "144",
                "proto",
                "tcp",
                "comment",
                "4931bafa-92d9-4521-9fc6-a77eee047530 rule B",
            ],
        );
        expect_with_args(
            &mut mock_runner,
            &[
                "--dry-run",
                "route",
                "allow",
                "in",
                "on",
                "bvbr0",
                "from",
                "ip.is.validated.before",
                "to",
                "192.168.0.7",
                "port",
                "77",
                "proto",
                "tcp",
                "comment",
                "4931bafa-92d9-4521-9fc6-a77eee047530 rule B",
            ],
        );
        expect_with_args(
            &mut mock_runner,
            &[
                "--dry-run",
                "route",
                "allow",
                "out",
                "on",
                "bvbr0",
                "from",
                "192.168.0.7",
                "to",
                "ip.is.validated.before",
                "port",
                "7",
                "comment",
                "4931bafa-92d9-4521-9fc6-a77eee047530 rule A",
            ],
        );
        expect_with_args(
            &mut mock_runner,
            &[
                "--dry-run",
                "route",
                "deny",
                "in",
                "on",
                "bvbr0",
                "from",
                "any",
                "to",
                "192.168.0.7",
                "comment",
                "4931bafa-92d9-4521-9fc6-a77eee047530 default in",
            ],
        );
        expect_with_args(
            &mut mock_runner,
            &[
                "--dry-run",
                "route",
                "allow",
                "out",
                "on",
                "bvbr0",
                "from",
                "192.168.0.7",
                "to",
                "any",
                "comment",
                "4931bafa-92d9-4521-9fc6-a77eee047530 default out",
            ],
        );

        expect_with_args(&mut mock_runner, &["status", "numbered"]);

        expect_with_args(
            &mut mock_runner,
            &[
                "route",
                "allow",
                "out",
                "on",
                "bvbr0",
                "from",
                "192.168.0.7",
                "to",
                "any",
                "port",
                "7",
                "comment",
                "4931bafa-92d9-4521-9fc6-a77eee047530 ",
            ],
        );
        expect_with_args(
            &mut mock_runner,
            &[
                "route",
                "allow",
                "out",
                "on",
                "bvbr0",
                "from",
                "192.168.0.7",
                "to",
                "any",
                "comment",
                "4931bafa-92d9-4521-9fc6-a77eee047530 no ports",
            ],
        );
        expect_with_args(
            &mut mock_runner,
            &[
                "route",
                "allow",
                "in",
                "on",
                "bvbr0",
                "from",
                "ip.is.validated.before",
                "to",
                "192.168.0.7",
                "port",
                "144",
                "proto",
                "tcp",
                "comment",
                "4931bafa-92d9-4521-9fc6-a77eee047530 rule B",
            ],
        );
        expect_with_args(
            &mut mock_runner,
            &[
                "route",
                "allow",
                "in",
                "on",
                "bvbr0",
                "from",
                "ip.is.validated.before",
                "to",
                "192.168.0.7",
                "port",
                "77",
                "proto",
                "tcp",
                "comment",
                "4931bafa-92d9-4521-9fc6-a77eee047530 rule B",
            ],
        );
        expect_with_args(
            &mut mock_runner,
            &[
                "route",
                "allow",
                "out",
                "on",
                "bvbr0",
                "from",
                "192.168.0.7",
                "to",
                "ip.is.validated.before",
                "port",
                "7",
                "comment",
                "4931bafa-92d9-4521-9fc6-a77eee047530 rule A",
            ],
        );
        expect_with_args(
            &mut mock_runner,
            &[
                "route",
                "deny",
                "in",
                "on",
                "bvbr0",
                "from",
                "any",
                "to",
                "192.168.0.7",
                "comment",
                "4931bafa-92d9-4521-9fc6-a77eee047530 default in",
            ],
        );
        expect_with_args(
            &mut mock_runner,
            &[
                "route",
                "allow",
                "out",
                "on",
                "bvbr0",
                "from",
                "192.168.0.7",
                "to",
                "any",
                "comment",
                "4931bafa-92d9-4521-9fc6-a77eee047530 default out",
            ],
        );

        apply_firewall_config_with(config, mock_runner).await?;
        Ok(())
    }
}

use async_trait::async_trait;
use babel_api::config::firewall::{Config, Protocol, Rule};
use eyre::{bail, Result};
use serde_variant::to_variant_name;

pub async fn apply_firewall_config(config: Config) -> Result<()> {
    apply_firewall_config_with(config, SysRunner).await
}

#[async_trait]
trait UfwRunner {
    async fn run<'a>(&self, args: &[&'a str]) -> Result<()>;
}

struct SysRunner;

#[async_trait]
impl UfwRunner for SysRunner {
    async fn run<'a>(&self, args: &[&'a str]) -> Result<()> {
        let output = tokio::process::Command::new("ufw")
            .args(args)
            .output()
            .await?;
        if !output.status.success() {
            let args_str = args.join(" ");
            bail!("Failed to run command 'ufw {args_str}', got output: `{output:?}`");
        }
        Ok(())
    }
}

async fn apply_firewall_config_with(config: Config, runner: impl UfwRunner) -> Result<()> {
    if config.enabled {
        // first convert config to convenient structure
        let rule_args = RuleArgs::from_rules(&config.rules);
        // dry-run rules to make sure they are valid
        for args in &rule_args {
            dry_run(&runner, &args.into()).await?;
        }
        //finally reset and apply whole firewall config
        runner.run(&["--force", "reset"]).await?;
        runner.run(&["enable"]).await?;
        runner
            .run(&["default", variant_to_string(&config.default_in), "incoming"])
            .await?;
        runner
            .run(&[
                "default",
                variant_to_string(&config.default_out),
                "outgoing",
            ])
            .await?;
        // and actually apply rules
        for args in &rule_args {
            runner.run(&args.into()).await?;
        }
    } else {
        runner.run(&["disable"]).await?;
    }
    Ok(())
}

struct RuleArgs<'a> {
    policy: &'a str,
    direction: &'a str,
    protocol: &'a str,
    ips: &'a str,
    port: Option<String>, // no port means - no port argument passed at all i.e. rule apply for all ports
    name: &'a str,
}

impl<'a> RuleArgs<'a> {
    fn from_rules(rules: &'a [Rule]) -> Vec<Self> {
        let mut rule_args = Vec::default();
        for rule in rules.iter().rev() {
            let proto = rule.protocol.as_ref();
            if rule.ports.is_empty() {
                rule_args.push(Self {
                    policy: variant_to_string(&rule.action),
                    direction: variant_to_string(&rule.direction),
                    protocol: variant_to_string(proto.unwrap_or(&Protocol::Both)),
                    ips: rule.ips.as_ref().map_or("any", |ip| ip.as_str()),
                    port: None,
                    name: rule.name.as_str(),
                });
            } else {
                for port in &rule.ports {
                    rule_args.push(Self {
                        policy: variant_to_string(&rule.action),
                        direction: variant_to_string(&rule.direction),
                        protocol: variant_to_string(proto.unwrap_or(&Protocol::Both)),
                        ips: rule.ips.as_ref().map_or("any", |ip| ip.as_str()),
                        port: Some(port.to_string()),
                        name: rule.name.as_str(),
                    });
                }
            }
        }
        rule_args
    }

    fn into(&self) -> Vec<&str> {
        let mut args = vec![
            self.policy,
            self.direction,
            "proto",
            self.protocol,
            "from",
            self.ips,
        ];
        if let Some(port) = &self.port {
            args.push("port");
            args.push(port.as_str());
        }
        if !self.name.is_empty() {
            args.push("comment");
            args.push(self.name);
        }
        args
    }
}

fn variant_to_string<T: serde::ser::Serialize>(variant: &T) -> &str {
    // `to_variant_name()` may fail only with `UnsupportedType` which shall not happen,
    // so it is safe to unwrap here.
    to_variant_name(variant).unwrap()
}

async fn dry_run(runner: &impl UfwRunner, args: &[&str]) -> Result<()> {
    runner.run(&[&["--dry-run"], args].concat()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use babel_api::config::firewall::{Action, Direction};
    use mockall::*;

    mock! {
        pub TestRunner {}

        #[async_trait]
        impl UfwRunner for TestRunner {
            async fn run<'a>(&self, args: &[&'a str]) -> Result<()>;
        }
    }

    fn expect_with_args(mock_runner: &mut MockTestRunner, expected_args: &'static [&str]) {
        mock_runner
            .expect_run()
            .once()
            .withf(move |args| args == expected_args)
            .returning(|_| Ok(()));
    }

    #[tokio::test]
    async fn test_run_failed() -> Result<()> {
        let config = Config {
            enabled: false,
            default_in: Action::Allow,
            default_out: Action::Allow,
            rules: vec![],
        };
        let mut mock_runner = MockTestRunner::new();
        mock_runner
            .expect_run()
            .once()
            .withf(|args| args == ["disable"])
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
    async fn test_disable() -> Result<()> {
        let config = Config {
            enabled: false,
            default_in: Action::Allow,
            default_out: Action::Allow,
            rules: vec![],
        };
        let mut mock_runner = MockTestRunner::new();
        expect_with_args(&mut mock_runner, &["disable"]);

        apply_firewall_config_with(config, mock_runner).await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_no_rules() -> Result<()> {
        let config = Config {
            enabled: true,
            default_in: Action::Deny,
            default_out: Action::Allow,
            rules: vec![],
        };
        let mut mock_runner = MockTestRunner::new();
        expect_with_args(&mut mock_runner, &["--force", "reset"]);
        expect_with_args(&mut mock_runner, &["enable"]);
        expect_with_args(&mut mock_runner, &["default", "deny", "incoming"]);
        expect_with_args(&mut mock_runner, &["default", "allow", "outgoing"]);

        apply_firewall_config_with(config, mock_runner).await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_with_rules() -> Result<()> {
        let config = Config {
            enabled: true,
            default_in: Action::Deny,
            default_out: Action::Reject,
            rules: vec![
                Rule {
                    name: "rule A".to_string(),
                    action: Action::Allow,
                    direction: Direction::Out,
                    protocol: None,
                    ips: None,
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
            ],
        };
        let mut mock_runner = MockTestRunner::new();
        expect_with_args(
            &mut mock_runner,
            &[
                "--dry-run",
                "allow",
                "out",
                "proto",
                "both",
                "from",
                "any",
                "port",
                "7",
            ],
        );
        expect_with_args(
            &mut mock_runner,
            &[
                "--dry-run",
                "allow",
                "out",
                "proto",
                "both",
                "from",
                "any",
                "comment",
                "no ports",
            ],
        );
        expect_with_args(
            &mut mock_runner,
            &[
                "--dry-run",
                "allow",
                "in",
                "proto",
                "tcp",
                "from",
                "ip.is.validated.before",
                "port",
                "144",
                "comment",
                "rule B",
            ],
        );
        expect_with_args(
            &mut mock_runner,
            &[
                "--dry-run",
                "allow",
                "in",
                "proto",
                "tcp",
                "from",
                "ip.is.validated.before",
                "port",
                "77",
                "comment",
                "rule B",
            ],
        );
        expect_with_args(
            &mut mock_runner,
            &[
                "--dry-run",
                "allow",
                "out",
                "proto",
                "both",
                "from",
                "any",
                "port",
                "7",
                "comment",
                "rule A",
            ],
        );

        expect_with_args(&mut mock_runner, &["--force", "reset"]);
        expect_with_args(&mut mock_runner, &["enable"]);
        expect_with_args(&mut mock_runner, &["default", "deny", "incoming"]);
        expect_with_args(&mut mock_runner, &["default", "reject", "outgoing"]);

        expect_with_args(
            &mut mock_runner,
            &["allow", "out", "proto", "both", "from", "any", "port", "7"],
        );
        expect_with_args(
            &mut mock_runner,
            &[
                "allow", "out", "proto", "both", "from", "any", "comment", "no ports",
            ],
        );
        expect_with_args(
            &mut mock_runner,
            &[
                "allow",
                "in",
                "proto",
                "tcp",
                "from",
                "ip.is.validated.before",
                "port",
                "144",
                "comment",
                "rule B",
            ],
        );
        expect_with_args(
            &mut mock_runner,
            &[
                "allow",
                "in",
                "proto",
                "tcp",
                "from",
                "ip.is.validated.before",
                "port",
                "77",
                "comment",
                "rule B",
            ],
        );
        expect_with_args(
            &mut mock_runner,
            &[
                "allow", "out", "proto", "both", "from", "any", "port", "7", "comment", "rule A",
            ],
        );

        apply_firewall_config_with(config, mock_runner).await?;
        Ok(())
    }
}

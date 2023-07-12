use clap::Parser;
use eyre::Result;
use std::time::Duration;

#[derive(Parser, Debug, Clone)]
#[clap(version, about, long_about = None)]
pub struct CmdArgs {
    /// S3 url
    pub s3_url: String,

    /// S3 bucket
    pub s3_bucket: String,

    /// S3 prefix
    pub s3_prefix: String,

    /// Number of slots in generated manifest.
    pub slots: usize,

    /// Presigned urls expire time (in seconds).
    #[clap(default_value = "86400")]
    pub expires_in_secs: u64,
}

/// Tool to generate upload manifest.
#[tokio::main]
async fn main() -> Result<()> {
    let cmd_args = CmdArgs::parse();
    let s3_config = aws_sdk_s3::Config::builder()
        .endpoint_url(&cmd_args.s3_url)
        .region(aws_sdk_s3::config::Region::new(std::env::var(
            "AWS_REGION",
        )?))
        .credentials_provider(aws_sdk_s3::config::Credentials::new(
            std::env::var("AWS_ACCESS_KEY_ID")?,
            std::env::var("AWS_SECRET_ACCESS_KEY")?,
            None,
            None,
            "Custom Provided Credentials",
        ))
        .build();
    let client = Client {
        cmd_args,
        client: aws_sdk_s3::Client::from_conf(s3_config),
    };

    let mut manifest = babel_api::engine::UploadManifest {
        slots: vec![],
        manifest_slot: babel_api::engine::Slot {
            key: format!("{}/manifest.json", client.cmd_args.s3_prefix),
            url: client.generate_presigned_url("manifest.json").await?,
        },
    };
    let mut n = 0;
    while n < client.cmd_args.slots {
        manifest.slots.push(babel_api::engine::Slot {
            key: format!("{}/data.part_{}", client.cmd_args.s3_prefix, n),
            url: client
                .generate_presigned_url(&format!("data.part_{}", n))
                .await?,
        });
        n += 1;
    }
    println!("{}", serde_json::to_string_pretty(&manifest)?);
    Ok(())
}

struct Client {
    cmd_args: CmdArgs,
    client: aws_sdk_s3::Client,
}

impl Client {
    async fn generate_presigned_url(&self, filename: &str) -> Result<String> {
        Ok(self
            .client
            .put_object()
            .bucket(&self.cmd_args.s3_bucket)
            .key(&format!("{}/{}", &self.cmd_args.s3_prefix, filename))
            .presigned(aws_sdk_s3::presigning::PresigningConfig::expires_in(
                Duration::from_secs(self.cmd_args.expires_in_secs),
            )?)
            .await?
            .uri()
            .to_string())
    }
}

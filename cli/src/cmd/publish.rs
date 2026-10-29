use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use clap::{ArgMatches, Args, FromArgMatches};
use edge::util::sha256::{hex_encode, sha256};
use ring::signature::Ed25519KeyPair;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::host::site;
use crate::kit::{Command, Field, Kit};
use crate::manifest::string;

/* the manifest keys a registry lists a package under in save order */
pub const FIELDS: &[Field] = &[
    Field { name: "name", check: string, reserves: &[] },
    Field { name: "version", check: string, reserves: &[] },
    Field { name: "description", check: string, reserves: &[] },
    Field { name: "repository", check: string, reserves: &[] },
];

#[derive(Args)]
struct PublishArgs {
    /// The .edge to publish, packed by edge build.
    artifact: PathBuf,
}

/* `publish` sending a packed bundle to the registry */
pub struct Publish;

impl Command for Publish {
    fn spec(&self, _kit: &Kit) -> clap::Command {
        PublishArgs::augment_args(clap::Command::new("publish").about("Send a packed .edge to the registry"))
    }

    fn summary(&self, _kit: &Kit) -> (String, String) {
        ("publish <file>".to_string(), "Send a packed .edge to the registry".to_string())
    }

    fn run(&self, kit: &Kit, m: &ArgMatches) -> Result<i32> {
        run(kit, &PublishArgs::from_arg_matches(m)?.artifact)?;
        Ok(0)
    }
}

/* the artifact is the whole request so the registry reads every field from the bytes */
fn run(kit: &Kit, artifact: &Path) -> Result<()> {
    let token = std::env::var("EDGE_TOKEN").map_err(|_| anyhow!("set EDGE_TOKEN to a token from edgelang.dev/settings#tokens"))?;

    let bytes = fs::read(artifact).with_context(|| format!("reading {}", artifact.display()))?;
    let name = artifact.file_name().unwrap_or(artifact.as_os_str()).to_string_lossy().into_owned();
    if let Some(path) = javascript_in(&bytes) {
        bail!("{name} carries '{path}', which is JavaScript, ship a .{} or a .wasm", kit.descriptor().extension);
    }

    let authorization = signature(&token, &bytes, SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())?;
    let sp = crate::ui::spinner(&format!("publishing {name}"));

    match send(&authorization, &bytes) {
        Ok(published) => {
            sp.done(&format!("published {} {}", published.name, published.version));
            crate::ui::note(&format!("add it with  {} add {}", kit.name(), published.name));
            crate::ui::note(&published.url);
            Ok(())
        }
        // versions are never overwritten and exit code 2 tells a script this from a failure
        Err(e) if already_published(&e.to_string()) => {
            sp.fail(&e.to_string());
            std::process::exit(2)
        }
        Err(e) => {
            sp.fail(&format!("failed to publish {name}"));
            Err(e)
        }
    }
}

// the words of the registry for a version it already holds
fn already_published(error: &str) -> bool {
    error.ends_with("is already published.")
}

/* a JavaScript file in the bundle that no host would load and so blocks publishing */
fn javascript_in(artifact: &[u8]) -> Option<String> {
    let files = crate::pack::Bundle::decode(artifact).map(|b| b.files).unwrap_or_default();
    files.into_iter().map(|f| f.path).find(|path| edge::deps::rules::javascript(path))
}

/* the secret signs instead of travelling since the registry keeps only its public key */
fn signature(token: &str, artifact: &[u8], at: u64) -> Result<String> {
    let (id, secret) = token.strip_prefix("edge_pat_").and_then(|rest| rest.split_once('.')).ok_or_else(|| anyhow!("EDGE_TOKEN is not a token from edgelang.dev/settings#tokens"))?;
    let key = Ed25519KeyPair::from_seed_unchecked(&sha256(secret.as_bytes())).map_err(|e| anyhow!("deriving the signing key: {e}"))?;
    let signed = format!("edge publish\n{id}\n{at}\n{}", hex_encode(&sha256(artifact)));
    Ok(format!("Edge {id}.{at}.{}", URL_SAFE_NO_PAD.encode(key.sign(signed.as_bytes()))))
}

/* the name and version as the registry read them from the artifact */
struct Published {
    name: String,
    version: String,
    url: String
}

/* the artifact as it sits on disk is the whole body */
fn send(authorization: &str, artifact: &[u8]) -> Result<Published> {
    // a refusal still carries a body where the registry says what went wrong
    let mut response = ureq::post(site("/api/publish").map_err(|e| anyhow!(e))?.as_str())
        .config()
        .http_status_as_error(false)
        .timeout_connect(Some(crate::host::CONNECT_TIMEOUT))
        .timeout_global(Some(crate::host::FETCH_TIMEOUT))
        .build()
        .header("authorization", authorization)
        .header("content-type", "application/octet-stream")
        .send(artifact)
        .map_err(|e| anyhow!("reaching the registry: {e}"))?;

    let status = response.status().as_u16();
    let text = response.body_mut().read_to_string().context("reading the registry's answer")?;
    let answer: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();

    let field = |key: &str| answer.get(key).and_then(|v| v.as_str()).map(str::to_string);

    match (field("name"), field("version"), field("url")) {
        (Some(name), Some(version), Some(url)) if status < 300 => Ok(Published { name, version, url }),
        _ => bail!("{}", field("error").unwrap_or_else(|| format!("the registry refused it with {status}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack::{Bundle, Entry};

    #[test]
    fn a_bundle_carrying_javascript_is_caught_before_it_is_sent() {
        let packed = |path: &str| Bundle { entry: "main.mini".to_string(), files: vec![Entry { path: "main.mini".to_string(), bytes: b"print(1)".to_vec() }, Entry { path: path.to_string(), bytes: Vec::new() }] }.encode();
        assert_eq!(javascript_in(&packed("lib/chart.js")).as_deref(), Some("lib/chart.js"));
        assert_eq!(javascript_in(&packed("https://x/y.mjs")).as_deref(), Some("https://x/y.mjs"));
        assert_eq!(javascript_in(&packed("util.mini")), None);
        assert_eq!(javascript_in(b"not a bundle"), None);
    }

    #[test]
    fn the_signature_is_the_one_webcrypto_makes_for_the_registry() {
        let token = "edge_pat_abcdefgh.Rk9vYmFyYmF6cXV4cXV1eGNvcmdlZ3JhdWx0Z2FycGw";
        assert_eq!(signature(token, b"EDGEPKG\x01", 1_700_000_000).unwrap(), "Edge abcdefgh.1700000000.L7yfHbh4meiykXFNQrVFfe0SfY5h4qpSWQyLUUqR0lH-mWq6fH8AoF_JBS6xBFISTT9U-IlHsDyqz38gwGnfAA");
        assert!(signature("not-a-token", b"", 0).is_err());
    }

    #[test]
    fn only_a_version_already_published_takes_exit_code_2() {
        assert!(already_published("greet 0.1.0 is already published."));
        assert!(!already_published("The name greet belongs to someone else."));
    }
}

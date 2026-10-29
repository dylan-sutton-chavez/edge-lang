use anyhow::{anyhow, bail, Context, Result};
use std::fs;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use crate::kit::{Descriptor, Field};

// every page lands under this prefix in the bundle so no import resolves to one
pub const PREFIX: &str = "@docs/";

/* the manifest key naming the folder a package ships its pages in */
pub const FIELD: Field = Field { name: "docs", check: crate::manifest::string, reserves: &[] };

/* every page under the declared docs dir keyed for the bundle or none if undeclared */
pub fn collect(project: &Path, declared: Option<&str>, d: &Descriptor) -> Result<Vec<(String, Vec<u8>)>> {
    let Some(declared) = declared else { return Ok(Vec::new()) };
    let root = project.join(relative(declared)?);
    if !root.is_dir() {
        bail!("edge.json declares docs at '{declared}' and no directory is there");
    }
    let mut found = pages(&root, "", &mut HashSet::new())?;
    if found.is_empty() {
        bail!("no .mdx pages under '{declared}', write one or drop the docs key");
    }
    found.sort();
    let mut out = Vec::with_capacity(found.len());
    for page in found {
        let bytes = fs::read(root.join(&page)).with_context(|| format!("reading {declared}/{page}"))?;
        let text = core::str::from_utf8(&bytes).map_err(|_| anyhow!("'{page}' is not valid UTF-8"))?;
        check(&page, text, d)?;
        out.push((format!("{PREFIX}{page}"), bytes));
    }
    Ok(out)
}

/* the declared path made plain and relative with the guard bundle paths get on read */
fn relative(declared: &str) -> Result<String> {
    let trimmed = declared.trim_start_matches("./").trim_end_matches('/');
    if !edge::deps::rules::plain(trimmed) {
        bail!("docs '{declared}' must be a relative path inside the project");
    }
    Ok(trimmed.to_string())
}

/* page paths below the docs root skipping hidden entries as the project walk does */
fn pages(dir: &Path, prefix: &str, seen: &mut HashSet<PathBuf>) -> Result<Vec<String>> {
    let mut found = Vec::new();
    if !crate::host::first_visit(seen, dir) {
        return Ok(found);
    }
    let entries = fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            found.extend(pages(&path, &format!("{prefix}{name}/"), seen)?);
        } else if name.ends_with(".mdx") {
            found.push(format!("{prefix}{name}"));
        }
    }
    Ok(found)
}

// the boxes a `[!KIND]` marker opens with the first five drawn as GitHub draws them
const KINDS: [&str; 9] = ["NOTE", "TIP", "IMPORTANT", "WARNING", "CAUTION", "QUESTION", "CARDS", "BANNER", "COPY"];

// the Lucide release `make lucide` writes with every icon name a card can carry
#[derive(serde::Deserialize)]
struct Lucide {
    version: &'static str,
    #[serde(borrow)]
    icons: Vec<&'static str>,
}

static LUCIDE: LazyLock<Lucide> = LazyLock::new(|| serde_json::from_str(include_str!(concat!(env!("OUT_DIR"), "/lucide.json"))).expect("parsing lucide.json"));

/* the site renderer conventions so no package ships docs the site cannot lay out */
fn check(page: &str, text: &str, d: &Descriptor) -> Result<()> {
    let segments: Vec<&str> = page.split('/').collect();
    if segments.len() > 3 {
        bail!("'{page}' nests deeper than two folders, a section, its groups and their pages is all the renderer orders");
    }
    if !segments.iter().copied().all(ordered) {
        bail!("'{page}' needs a numeric prefix on every segment, like '01-reference/02-cli.mdx'");
    }
    let mut open: Option<&str> = None;
    let mut closed: Option<&str> = None;
    let mut headings = 0usize;
    let mut quote = Quote::Out;
    // the lines of the manifest block being read and checked once its fence closes
    let mut manifest = String::new();
    for line in front(page, text)?.lines() {
        if open.is_none() {
            quote.read(page, line)?;
        }
        if let Some(rest) = line.trim().strip_prefix("```") {
            match open.take() {
                Some(lang) => {
                    if lang == d.manifest_fence && !object(&manifest) {
                        bail!("'{page}' has an {} block that is not a JSON object", d.manifest_fence);
                    }
                    closed = Some(lang);
                }
                None => {
                    let lang = rest.trim();
                    if lang == "output" && closed != Some(d.example_fence) {
                        bail!("'{page}' has an output block that follows no {} block", d.example_fence);
                    }
                    if closed == Some(d.manifest_fence) && lang != d.example_fence {
                        return Err(lone(page, d));
                    }
                    open = Some(lang);
                    closed = None;
                    manifest.clear();
                }
            }
            continue;
        }
        if open == Some(d.manifest_fence) {
            manifest.push_str(line);
            manifest.push('\n');
        }
        if open.is_none() {
            if line.starts_with("# ") {
                headings += 1;
            }
            if let Some(tag) = tagged(line) {
                bail!("'{page}' writes the raw HTML '{tag}', and a page is markdown the site renders itself");
            }
            // blank lines keep two fences adjacent but prose between them does not
            if !line.trim().is_empty() {
                if closed == Some(d.manifest_fence) {
                    return Err(lone(page, d));
                }
                closed = None;
            }
        }
    }
    if open.is_some() {
        bail!("'{page}' leaves a code fence unterminated");
    }
    quote.end(page)?;
    if closed == Some(d.manifest_fence) {
        return Err(lone(page, d));
    }
    if headings != 1 {
        bail!("'{page}' has {headings} top-level headings, the renderer needs exactly one");
    }
    Ok(())
}

/* where a prose line stands among blockquotes so a box is read whole before drawing */
enum Quote<'a> {
    Out,
    Plain,
    Boxed { kind: &'a str, lines: usize },
}

impl<'a> Quote<'a> {
    fn read(&mut self, page: &str, line: &'a str) -> Result<()> {
        let Some(rest) = line.strip_prefix('>') else {
            if let Quote::Boxed { kind, .. } = *self
                && !line.trim().is_empty()
            {
                bail!("'{page}' runs prose on from its [!{kind}] box, a blank line ends it");
            }
            return self.end(page);
        };
        let rest = rest.trim();
        match (&mut *self, marker(rest)) {
            (Quote::Out, Some((kind, beside))) => {
                if !KINDS.contains(&kind) {
                    bail!("'{page}' opens a [!{kind}] box, and a box is one of {}", KINDS.join(", "));
                }
                // only a question or banner or copy box writes its text beside the marker
                let what = match kind {
                    "QUESTION" => "question",
                    "BANNER" => "title",
                    "COPY" => "text",
                    _ => "",
                };
                match (what.is_empty(), beside.is_empty()) {
                    (false, true) => bail!("'{page}' opens a [!{kind}] box with no {what} beside the marker"),
                    (true, false) => bail!("'{page}' writes text beside [!{kind}], the box holds it on the lines below"),
                    _ => {}
                }
                if kind == "COPY" && beside.contains('`') {
                    bail!("'{page}' writes backticks in its [!COPY] box, the box copies its text as written");
                }
                *self = Quote::Boxed { kind, lines: 0 };
            }
            (_, Some((kind, _))) => bail!("'{page}' writes [!{kind}] inside a quote, a marker only ever opens one"),
            (Quote::Boxed { kind, lines }, None) if !rest.is_empty() => {
                *lines += 1;
                shaped(page, kind, *lines, rest)?;
            }
            (Quote::Out, None) => *self = Quote::Plain,
            _ => {}
        }
        Ok(())
    }

    fn end(&mut self, page: &str) -> Result<()> {
        if let Quote::Boxed { kind, lines: 0 } = *self
            && kind != "COPY"
        {
            bail!("'{page}' leaves its [!{kind}] box empty");
        }
        *self = Quote::Out;
        Ok(())
    }
}

// the `[!KIND]` opening a quote line and the text beside it
fn marker(rest: &str) -> Option<(&str, &str)> {
    let (kind, beside) = rest.strip_prefix("[!")?.split_once(']')?;
    (!kind.is_empty() && kind.bytes().all(|b| b.is_ascii_alphabetic())).then(|| (kind, beside.trim()))
}

/* holds a box line to the shape the site lays out for cards and banners */
fn shaped(page: &str, kind: &str, lines: usize, rest: &str) -> Result<()> {
    let href = match kind {
        "CARDS" => {
            let Some((icon, href)) = card(rest) else {
                bail!("'{page}' has the card '{rest}', a card is - `icon` [Title](link) and a description");
            };
            if !LUCIDE.icons.contains(&icon) {
                bail!("'{page}' gives a card the icon '{icon}', which Lucide {} does not draw, see https://lucide.dev/icons", LUCIDE.version);
            }
            href
        }
        "BANNER" => match button(rest) {
            Some(href) if lines == 1 => href,
            _ => bail!("'{page}' has the banner line '{rest}', a banner holds one [Action](link) below its title"),
        },
        "COPY" => bail!("'{page}' writes '{rest}' below [!COPY], the text to copy sits beside the marker"),
        _ => return Ok(()),
    };
    if !linked(href) {
        bail!("'{page}' links a box to '{href}', which is neither https nor a path on the site");
    }
    Ok(())
}

// the icon and link of a card line
fn card(rest: &str) -> Option<(&str, &str)> {
    let (icon, rest) = rest.strip_prefix("- `")?.split_once("` [")?;
    let (title, rest) = rest.split_once("](")?;
    let (href, about) = rest.split_once(") ")?;
    (!title.is_empty() && !about.trim().is_empty()).then_some((icon, href))
}

// the link of an `[Action](link)` line
fn button(rest: &str) -> Option<&str> {
    let (label, href) = rest.strip_prefix('[')?.strip_suffix(')')?.split_once("](")?;
    (!label.is_empty()).then_some(href)
}

/* https or a site path since a link without a colon names no scheme */
fn linked(href: &str) -> bool {
    !href.contains(char::is_whitespace) && (href.starts_with("https://") || !(href.contains(':') || href.starts_with("//")))
}

/* a manifest block no example block follows so no example would run under it */
fn lone(page: &str, d: &Descriptor) -> anyhow::Error {
    anyhow!("'{page}' has an {} block that no {} block follows", d.manifest_fence, d.example_fence)
}

// a manifest block holds one JSON object as the edge.json its example runs under
fn object(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text).is_ok_and(|value| value.is_object())
}

/* the first HTML tag in prose since a raw tag would run on the registry */
fn tagged(line: &str) -> Option<String> {
    let mut prose = String::with_capacity(line.len());
    let mut code = false;
    for c in line.chars() {
        match c {
            '`' => code = !code,
            _ if !code => prose.push(c),
            _ => {}
        }
    }

    // every `<` is a candidate so `a < b` cannot hide a later tag
    for (at, _) in prose.match_indices('<') {
        let rest = &prose[at + 1..];
        let closing = rest.starts_with('/');
        let name = if closing { &rest[1..] } else { rest };
        if !name.starts_with(|c: char| c.is_ascii_alphabetic()) {
            continue;
        }
        let end = name.find(|c: char| c.is_whitespace() || c == '>').unwrap_or(name.len());
        // a tag shows its closing bracket only when it closes right after its name
        let shut = if name[end..].starts_with('>') { ">" } else { "" };
        return Some(format!("<{}{}{shut}", if closing { "/" } else { "" }, &name[..end]));
    }

    None
}

/* the page past a frontmatter that holds a title and a description and closes */
fn front<'a>(page: &str, text: &'a str) -> Result<&'a str> {
    let Some(rest) = text.strip_prefix("---\n").or_else(|| text.strip_prefix("---\r\n")) else {
        bail!("'{page}' opens with no frontmatter, a page needs a title and a description");
    };
    let mut at = 0usize;
    let mut named = false;
    let mut described = false;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end();
        if trimmed == "---" {
            if !named || !described {
                bail!("'{page}' needs both a title and a description in its frontmatter");
            }
            return Ok(&rest[at + line.len()..]);
        }
        if !trimmed.is_empty() {
            let Some((key, value)) = trimmed.split_once(':') else {
                bail!("'{page}' has the frontmatter line '{trimmed}', which is no key and value");
            };
            if value.trim().is_empty() {
                bail!("'{page}' leaves the frontmatter '{}' empty", key.trim());
            }
            match key.trim() {
                "title" => named = true,
                "description" => described = true,
                _ => {}
            }
        }
        at += line.len();
    }
    bail!("'{page}' leaves its frontmatter unterminated")
}

/* a segment the renderer can order with digits then a separator as in `01-cli.mdx` */
fn ordered(segment: &str) -> bool {
    let rest = segment.trim_start_matches(|c: char| c.is_ascii_digit());
    rest.len() < segment.len() && (rest.starts_with('-') || rest.starts_with('_'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[derive(serde::Deserialize)]
    struct Case {
        name: String,
        files: BTreeMap<String, String>,
        #[serde(default)]
        error: Option<String>,
    }

    /* the corpus shared with the site renderer so neither side changes a rule alone */
    #[test]
    fn the_shared_corpus_agrees() {
        let cases: Vec<Case> = serde_json::from_str(include_str!("../../tests/cases/docs.json")).expect("parsing docs.json");
        assert!(!cases.is_empty());
        for case in cases {
            let dir = tempfile::tempdir().unwrap();
            for (path, body) in &case.files {
                let at = dir.path().join("docs").join(path);
                fs::create_dir_all(at.parent().unwrap()).unwrap();
                fs::write(at, body).unwrap();
            }
            let pages = case.files.keys().filter(|p| p.ends_with(".mdx")).count();
            match (&case.error, collect(dir.path(), Some("./docs"), &mini::DESCRIPTOR)) {
                (None, Ok(got)) => assert_eq!(got.len(), pages, "{}", case.name),
                (None, Err(e)) => panic!("{} should pass, got {e:#}", case.name),
                (Some(want), Err(e)) => {
                    let got = format!("{e:#}");
                    assert!(got.contains(want), "{} wanted '{want}', got '{got}'", case.name);
                }
                (Some(want), Ok(_)) => panic!("{} should fail with '{want}'", case.name),
            }
        }
    }

    const PAGE: &str = "---\ntitle: Cli\ndescription: Every command.\n---\n\n# Cli\n";

    #[test]
    fn a_page_lands_under_the_reserved_prefix() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("docs/01-reference")).unwrap();
        fs::write(dir.path().join("docs/01-reference/01-cli.mdx"), PAGE).unwrap();
        let got = collect(dir.path(), Some("docs"), &mini::DESCRIPTOR).unwrap();
        assert_eq!(got[0].0, "@docs/01-reference/01-cli.mdx");
    }

    #[test]
    fn no_docs_key_collects_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(collect(dir.path(), None, &mini::DESCRIPTOR).unwrap().is_empty());
    }

    #[test]
    fn a_path_outside_the_project_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        for bad in ["../docs", "/etc", "https://example.com/docs", "./"] {
            let err = format!("{:#}", collect(dir.path(), Some(bad), &mini::DESCRIPTOR).expect_err(bad));
            assert!(err.contains("must be a relative path"), "{bad} got '{err}'");
        }
    }

    /* a fence naming a repo file stays empty for the site to fill */
    #[test]
    fn every_included_file_is_in_the_repo_and_never_copied() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut included = Vec::new();
        for page in pages(&repo.join("docs"), "", &mut HashSet::new()).unwrap() {
            let text = fs::read_to_string(repo.join("docs").join(&page)).unwrap();
            let mut lines = text.lines();
            while let Some(line) = lines.next() {
                let named = line.trim().strip_prefix("```").and_then(|info| info.split_whitespace().find_map(|word| word.strip_prefix("file=")));
                let Some(file) = named else { continue };
                assert!(repo.join(file).is_file(), "'{page}' includes '{file}', which the repo does not hold");
                assert_eq!(lines.next().map(str::trim), Some("```"), "'{page}' copies '{file}' into its fence, the site fills it");
                included.push(file.to_string());
            }
        }
        assert!(included.iter().any(|f| f == "examples/net/net.rs") && included.iter().any(|f| f == "examples/net/net.ts"), "{included:?}");
    }
}

from __future__ import annotations

from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def replace_once(path: str, old: str, new: str) -> None:
    target = ROOT / path
    text = target.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected one replacement anchor, found {count}")
    target.write_text(text.replace(old, new, 1))


replace_once(
    "src/cli.rs",
    '''    Plan {
        /// Emit machine-readable JSON rather than the human summary
        #[arg(long, env = "ZED_PKG_RELEASE_JSON")]
        json: bool,
    },''',
    '''    Plan {
        /// Emit machine-readable JSON rather than the human summary
        #[arg(
            long,
            env = "ZED_PKG_RELEASE_JSON",
            conflicts_with = "html"
        )]
        json: bool,
        /// Write a self-contained browser report instead of terminal output
        #[arg(
            long,
            env = "ZED_PKG_RELEASE_HTML",
            value_name = "PATH",
            conflicts_with = "json"
        )]
        html: Option<PathBuf>,
    },''',
)

replace_once(
    "src/main.rs",
    '''            ReleaseCmd::Plan { json } => release::plan(&cwd, json),''',
    '''            ReleaseCmd::Plan { json, html } => release::plan(&cwd, json, html.as_deref()),''',
)

replace_once(
    ".cli-flags.toml",
    '''[commands.release.commands.plan.flags.release_json]
env = "ZED_PKG_RELEASE_JSON"
aliases = ["json"]
type = "bool"
default = "false"
help = "Emit the release plan as JSON."
''',
    '''[commands.release.commands.plan.flags.release_json]
env = "ZED_PKG_RELEASE_JSON"
aliases = ["json"]
type = "bool"
default = "false"
help = "Emit the release plan as JSON."

[commands.release.commands.plan.flags.release_html]
env = "ZED_PKG_RELEASE_HTML"
aliases = ["html"]
type = "string"
help = "Write a self-contained browser release-plan report."
''',
)

replace_once(
    "src/release.rs",
    '''use std::fs;
use std::path::{Path, PathBuf};''',
    '''use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};''',
)

html_support = r'''
fn escape_html(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            character if character.is_control() && !matches!(character, '\n' | '\r' | '\t') => {
                escaped.push('\u{fffd}');
            }
            character => escaped.push(character),
        }
    }
    escaped
}

fn repository_link(value: &str) -> Option<String> {
    let url = reqwest::Url::parse(value).ok()?;
    if !matches!(url.scheme(), "https" | "http") || !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    Some(escape_html(value))
}

fn push_cell(output: &mut String, value: &str) {
    output.push_str("<td>");
    output.push_str(&escape_html(value));
    output.push_str("</td>");
}

fn push_optional_cell(output: &mut String, value: Option<&str>, fallback: &str) {
    push_cell(output, value.unwrap_or(fallback));
}

pub fn render_html(plan: &ReleasePlan) -> String {
    const NONCE: &str = "zed-release-plan";
    let mut output = String::with_capacity(24_000);
    output.push_str("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">");
    output.push_str("<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">");
    output.push_str("<meta name=\"color-scheme\" content=\"light dark\">");
    output.push_str("<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'nonce-");
    output.push_str(NONCE);
    output.push_str("'; script-src 'nonce-");
    output.push_str(NONCE);
    output.push_str("'; img-src data:; connect-src 'none'; font-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'\">");
    output.push_str("<title>");
    output.push_str(&escape_html(&format!(
        "{} {} release plan",
        plan.source.package, plan.source.version
    )));
    output.push_str("</title><style nonce=\"");
    output.push_str(NONCE);
    output.push_str(r#"\">
:root{color-scheme:light dark;font-family:ui-sans-serif,system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;line-height:1.45;--surface:color-mix(in srgb,Canvas 94%,CanvasText 6%);--border:color-mix(in srgb,CanvasText 20%,transparent);--accent:#5669ff}*{box-sizing:border-box}body{margin:0;background:Canvas;color:CanvasText}main,footer{width:min(1180px,calc(100% - 2rem));margin-inline:auto}main{padding:3rem 0 2rem}header{max-width:80ch}.eyebrow{margin:0;color:var(--accent);font-weight:750;letter-spacing:.08em;text-transform:uppercase}h1{font-size:clamp(2.2rem,6vw,4.8rem);line-height:1.02;margin:.35rem 0 1rem}h2{margin-top:2.5rem}.source{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:.75rem;padding:1rem;border:1px solid var(--border);border-radius:.8rem;background:var(--surface)}.source dt{font-weight:750}.source dd{margin:0;overflow-wrap:anywhere}.counts{display:grid;grid-template-columns:repeat(3,minmax(0,1fr));gap:.75rem;margin:1rem 0}.count{padding:1rem;border:1px solid var(--border);border-radius:.8rem;background:var(--surface)}.count strong{display:block;font-size:2rem}.filter{position:sticky;top:0;z-index:1;padding:1rem 0;background:Canvas}.filter label{display:block;font-weight:750;margin-bottom:.35rem}.filter input{width:min(100%,38rem);font:inherit;padding:.7rem .8rem;border:1px solid var(--border);border-radius:.5rem;background:Canvas;color:CanvasText}.table-wrap{overflow:auto;border:1px solid var(--border);border-radius:.8rem}table{width:100%;border-collapse:collapse;min-width:48rem}caption{text-align:left;font-size:1.25rem;font-weight:800;padding:1rem;background:var(--surface)}th,td{text-align:left;vertical-align:top;padding:.7rem .8rem;border-top:1px solid var(--border);overflow-wrap:anywhere}th{font-size:.82rem;text-transform:uppercase;letter-spacing:.04em}.empty{padding:1rem;border:1px dashed var(--border);border-radius:.8rem}.muted{color:color-mix(in srgb,CanvasText 68%,transparent)}code{font-family:ui-monospace,SFMono-Regular,Consolas,"Liberation Mono",monospace}a{color:var(--accent)}[hidden]{display:none!important}footer{padding:1.5rem 0 3rem;border-top:1px solid var(--border)}:focus-visible{outline:3px solid color-mix(in srgb,var(--accent) 70%,white);outline-offset:2px}@media(max-width:700px){main,footer{width:min(100% - 1rem,44rem)}main{padding-top:1.25rem}.source,.counts{grid-template-columns:1fr}.filter{position:static}.table-wrap{max-width:100%}h1{font-size:2.25rem}}
</style></head><body><main><header><p class="eyebrow">Credential-free coordinated release</p><h1>Release plan</h1><p><code>"#);
    output.push_str(&escape_html(&plan.release_set));
    output.push_str("</code></p></header><dl class=\"source\"><div><dt>Source package</dt><dd>");
    output.push_str(&escape_html(&plan.source.package));
    output.push_str("</dd></div><div><dt>Version</dt><dd>");
    output.push_str(&escape_html(&plan.source.version));
    output.push_str("</dd></div><div><dt>VCS tag</dt><dd><code>");
    output.push_str(&escape_html(&plan.source.vcs_tag));
    output.push_str("</code></dd></div><div><dt>Repository</dt><dd>");
    if let Some(href) = repository_link(&plan.source.repository) {
        output.push_str("<a rel=\"noreferrer\" href=\"");
        output.push_str(&href);
        output.push_str("\">");
        output.push_str(&escape_html(&plan.source.repository));
        output.push_str("</a>");
    } else {
        output.push_str("<code>");
        output.push_str(&escape_html(&plan.source.repository));
        output.push_str("</code>");
    }
    output.push_str("</dd></div></dl><section class=\"counts\" aria-label=\"Artifact counts\">");
    for (kind, label, count) in [
        ("zed", "Zed artifacts", plan.zed.len()),
        ("native", "Native artifacts", plan.native.len()),
        ("forge", "Forge mirrors", plan.forge.len()),
    ] {
        output.push_str("<div class=\"count\"><span>");
        output.push_str(label);
        output.push_str("</span><strong data-count=\"");
        output.push_str(kind);
        output.push_str("\">");
        output.push_str(&count.to_string());
        output.push_str("</strong></div>");
    }
    let total = plan.zed.len() + plan.native.len() + plan.forge.len();
    output.push_str("</section><section class=\"filter\" aria-labelledby=\"filter-heading\"><h2 id=\"filter-heading\">Review artifacts</h2><label for=\"artifact-filter\">Filter release artifacts</label><input id=\"artifact-filter\" type=\"search\" autocomplete=\"off\" placeholder=\"Package, registry, target, directory…\"><p id=\"filter-status\" class=\"muted\" role=\"status\" aria-live=\"polite\">");
    output.push_str(&format!("{total} of {total} artifacts"));
    output.push_str("</p></section><section aria-labelledby=\"zed-heading\"><h2 id=\"zed-heading\">Zed artifacts</h2><div class=\"table-wrap\"><table aria-label=\"Zed artifacts\"><caption>Zed package artifacts</caption><thead><tr><th>Target</th><th>Package</th><th>Version</th><th>Directory</th></tr></thead><tbody>");
    for artifact in &plan.zed {
        output.push_str("<tr data-artifact-row><td>");
        output.push_str(&escape_html(artifact.target.as_deref().unwrap_or("repository")));
        output.push_str("</td>");
        push_cell(&mut output, &artifact.package);
        push_cell(&mut output, &artifact.version);
        push_cell(&mut output, &artifact.dir);
        output.push_str("</tr>");
    }
    output.push_str("</tbody></table></div></section><section aria-labelledby=\"native-heading\"><h2 id=\"native-heading\">Native registry artifacts</h2>");
    if plan.native.is_empty() {
        output.push_str("<p class=\"empty\">No native registry artifacts declared.</p>");
    } else {
        output.push_str("<div class=\"table-wrap\"><table aria-label=\"Native registry artifacts\"><caption>Native registry artifacts</caption><thead><tr><th>Target</th><th>Registry</th><th>Package</th><th>Version</th><th>VCS tag</th><th>Directory</th></tr></thead><tbody>");
        for artifact in &plan.native {
            output.push_str("<tr data-artifact-row>");
            push_cell(&mut output, &artifact.target);
            push_cell(&mut output, &artifact.registry);
            push_cell(&mut output, &artifact.package);
            push_cell(&mut output, &artifact.version);
            push_cell(&mut output, &artifact.vcs_tag);
            push_cell(&mut output, &artifact.dir);
            output.push_str("</tr>");
        }
        output.push_str("</tbody></table></div>");
    }
    output.push_str("</section><section aria-labelledby=\"forge-heading\"><h2 id=\"forge-heading\">Forge package mirrors</h2>");
    if plan.forge.is_empty() {
        output.push_str("<p class=\"empty\">No forge package mirrors declared.</p>");
    } else {
        output.push_str("<div class=\"table-wrap\"><table aria-label=\"Forge package mirrors\"><caption>Forge package mirrors</caption><thead><tr><th>Target</th><th>Registry</th><th>Format</th><th>Package</th><th>Version</th><th>VCS tag</th><th>Directory</th></tr></thead><tbody>");
        for artifact in &plan.forge {
            output.push_str("<tr data-artifact-row>");
            push_cell(&mut output, &artifact.target);
            push_cell(&mut output, &artifact.registry);
            push_cell(&mut output, &artifact.format);
            push_cell(&mut output, &artifact.package);
            push_cell(&mut output, &artifact.version);
            push_cell(&mut output, &artifact.vcs_tag);
            push_cell(&mut output, &artifact.dir);
            output.push_str("</tr>");
        }
        output.push_str("</tbody></table></div>");
    }
    output.push_str("</section></main><footer><p>Generated locally by <code>zed release plan --html</code>. No credentials, uploads, analytics, remote scripts, fonts, or styles are used.</p></footer><script nonce=\"");
    output.push_str(NONCE);
    output.push_str(r#"\">(() => {"use strict";const input=document.querySelector("#artifact-filter");const status=document.querySelector("#filter-status");const rows=Array.from(document.querySelectorAll("[data-artifact-row]"));const update=()=>{const query=input.value.trim().toLocaleLowerCase();let visible=0;for(const row of rows){const matches=!query||row.textContent.toLocaleLowerCase().includes(query);row.hidden=!matches;if(matches)visible+=1;}status.textContent=`${visible} of ${rows.length} artifacts`;};input.addEventListener("input",update);input.addEventListener("keydown",event=>{if(event.key==="Escape"&&input.value){input.value="";update();event.preventDefault();}});update();})();</script></body></html>"#);
    output
}

fn write_html_report(path: &Path, plan: &ReleasePlan) -> Result<()> {
    if fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        bail!("refusing to write release-plan HTML through symbolic link {}", path.display());
    }
    let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("creating release-plan report directory {}", parent.display()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating temporary release-plan report in {}", parent.display()))?;
    temporary
        .write_all(render_html(plan).as_bytes())
        .with_context(|| format!("writing temporary release-plan report for {}", path.display()))?;
    temporary
        .flush()
        .with_context(|| format!("flushing temporary release-plan report for {}", path.display()))?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("publishing release-plan report {}", path.display()))?;
    Ok(())
}

'''
replace_once("src/release.rs", "pub fn render_human(plan: &ReleasePlan) -> String {", html_support + "pub fn render_human(plan: &ReleasePlan) -> String {")

replace_once(
    "src/release.rs",
    '''pub fn plan(project: &Path, json: bool) -> Result<()> {
    let manifest = read_manifest(project)?;
    validate_native_manifests(project, &manifest)?;
    let plan = build_plan(&manifest);
    if json {
        println!("{}", serde_json::to_string_pretty(&plan)?);
    } else {
        print!("{}", render_human(&plan));
    }
    Ok(())
}''',
    '''pub fn plan(project: &Path, json: bool, html: Option<&Path>) -> Result<()> {
    if json && html.is_some() {
        bail!("--json and --html cannot be used together");
    }
    let manifest = read_manifest(project)?;
    validate_native_manifests(project, &manifest)?;
    let plan = build_plan(&manifest);
    if let Some(path) = html {
        write_html_report(path, &plan)?;
        println!("wrote {}", path.display());
    } else if json {
        println!("{}", serde_json::to_string_pretty(&plan)?);
    } else {
        print!("{}", render_human(&plan));
    }
    Ok(())
}''',
)

html_tests = r'''
    #[test]
    fn html_report_is_deterministic_escaped_and_self_contained() {
        let mut plan = build_plan(&polyglot_manifest());
        plan.release_set = "<script>alert('set')</script>&".to_string();
        plan.source.package = "acme/<clients>".to_string();
        plan.source.repository = "javascript:alert(1)".to_string();
        plan.zed[0].package = "<img src=x onerror=alert(1)>".to_string();

        let first = render_html(&plan);
        let second = render_html(&plan);
        assert_eq!(first, second);
        assert!(first.starts_with("<!doctype html>"));
        assert!(first.contains("&lt;script&gt;alert(&#39;set&#39;)&lt;/script&gt;&amp;"));
        assert!(first.contains("&lt;img src=x onerror=alert(1)&gt;"));
        assert!(!first.contains("<script>alert('set')</script>"));
        assert!(!first.contains("href=\"javascript:"));
        assert!(first.contains("default-src 'none'"));
        assert!(!first.contains("src=\"http"));
        assert!(first.contains("data-count=\"zed\">5</strong>"));
        assert!(first.contains("data-count=\"native\">4</strong>"));
        assert!(first.contains("data-count=\"forge\">4</strong>"));
    }

    #[test]
    fn html_report_has_explicit_empty_states() {
        let manifest = Manifest::parse(
            r#"
[package]
org = "acme"
name = "minimal"
version = "1.0.0"

[package.repository]
url = "https://github.com/acme/minimal"
"#,
        )
        .unwrap();
        let html = render_html(&build_plan(&manifest));
        assert!(html.contains("No native registry artifacts declared."));
        assert!(html.contains("No forge package mirrors declared."));
        assert!(html.contains("1 of 1 artifacts"));
    }

'''
replace_once(
    "src/release.rs",
    "    #[test]\n    fn polyglot_plan_is_deterministic_and_includes_native_routes() {",
    html_tests + "    #[test]\n    fn polyglot_plan_is_deterministic_and_includes_native_routes() {",
)

print("DEN-1301 release-plan HTML transformations applied")

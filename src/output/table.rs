use crate::types::SearchResponse;
use comfy_table::{modifiers::UTF8_ROUND_CORNERS, presets::UTF8_FULL, ContentArrangement, Table};
use owo_colors::OwoColorize;
use std::io::IsTerminal;

pub fn render(response: &SearchResponse) {
    let use_color = std::io::stdout().is_terminal();

    if response.results.is_empty() && response.answers.is_empty() {
        if use_color {
            eprintln!("{}", "No results found.".yellow());
        } else {
            eprintln!("No results found.");
        }
        return;
    }

    if response.metadata.cached {
        let age = response
            .metadata
            .cache_age_secs
            .map(|s| format!(" ({s}s old)"))
            .unwrap_or_default();
        if use_color {
            eprintln!("{}", format!("  cached result{age}").yellow());
        } else {
            eprintln!("  cached result{age}");
        }
    }

    // AI-synthesized answers (Perplexity/Tavily) — separate from web results.
    for answer in &response.answers {
        if use_color {
            println!(
                "{} {}",
                "answer".on_green().black().bold(),
                answer.provider.cyan()
            );
            println!("  {}", truncate(&answer.text, 600));
            println!();
        } else {
            println!("[answer via {}]", answer.provider);
            println!("  {}", truncate(&answer.text, 600));
            println!();
        }
    }

    for warning in &response.metadata.warnings {
        if use_color {
            eprintln!("  {} {}", "!".yellow().bold(), warning.yellow());
        } else {
            eprintln!("  ! {warning}");
        }
    }

    // Header
    if use_color {
        eprintln!(
            "\n{}  {} results for {}  [mode: {}]",
            "search".bold().cyan(),
            response.metadata.result_count.to_string().bold(),
            format!("\"{}\"", response.query).white().bold(),
            response.mode.green(),
        );
        eprintln!();
    }

    for (i, result) in response.results.iter().enumerate() {
        let num = format!(" {} ", i + 1);
        let title = &result.title;
        let url = &result.url;
        let snippet = truncate(&result.snippet, 200);
        let source = &result.source;

        if use_color {
            println!("{} {}", num.on_cyan().black().bold(), title.bold(),);
            println!("  {} {}", "->".dimmed(), url.blue().underline());
            if !snippet.is_empty() {
                println!("  {}", snippet.dimmed());
            }
            let mut meta_parts = vec![format!("via {}", source.cyan())];
            if let Some(pub_date) = &result.published {
                meta_parts.push(pub_date.dimmed().to_string());
            }
            println!("  {}", meta_parts.join("  "));
            println!();
        } else {
            println!("[{}] {}", i + 1, title);
            println!("    {}", url);
            if !snippet.is_empty() {
                println!("    {}", snippet);
            }
            println!("    [{}]", source);
            println!();
        }
    }

    // Footer
    if use_color {
        eprintln!(
            "{}",
            format!(
                "  {} results from {} in {}ms",
                response.metadata.result_count,
                response
                    .metadata
                    .providers_queried
                    .iter()
                    .map(|p| p.cyan().to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
                response.metadata.elapsed_ms,
            )
            .dimmed()
        );
    } else {
        eprintln!(
            "  {} results from {} in {}ms",
            response.metadata.result_count,
            response.metadata.providers_queried.join(", "),
            response.metadata.elapsed_ms,
        );
    }

    if !response.metadata.provider_failures.is_empty() {
        // Structured detail: which provider, why, and the underlying reason.
        for f in &response.metadata.provider_failures {
            let line = match f.http_status {
                Some(s) => format!(
                    "  failed: {} [{} · {}] {}",
                    f.provider,
                    f.category.as_str(),
                    s,
                    f.reason
                ),
                None => format!(
                    "  failed: {} [{}] {}",
                    f.provider,
                    f.category.as_str(),
                    f.reason
                ),
            };
            if use_color {
                eprintln!("{}", line.red());
            } else {
                eprintln!("{line}");
            }
        }
    } else if !response.metadata.providers_failed.is_empty() {
        // Fallback for envelopes without structured detail (e.g. replayed cache).
        let names = response.metadata.providers_failed.join(", ");
        if use_color {
            eprintln!("  {} {}", "failed:".red(), names.red());
        } else {
            eprintln!("  failed: {names}");
        }
    }
    eprintln!();
}

fn truncate(s: &str, max: usize) -> String {
    // Clean up: collapse whitespace, remove newlines
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c == '\n' || c == '\r' || c == '\t' {
                ' '
            } else {
                c
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    // Char-based (never slices across a UTF-8 boundary, which would panic).
    if cleaned.chars().count() <= max {
        cleaned
    } else {
        let kept: String = cleaned.chars().take(max.saturating_sub(1)).collect();
        format!("{kept}…")
    }
}

pub fn render_providers(providers: &[(String, bool, Vec<String>)]) {
    let use_color = std::io::stdout().is_terminal();

    if use_color {
        eprintln!("\n{}  Provider Status\n", "search".bold().cyan());
    }

    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_content_arrangement(ContentArrangement::Dynamic);

    table.set_header(vec!["Provider", "Status", "Capabilities"]);

    for (name, configured, caps) in providers {
        let status = if *configured {
            if use_color {
                "OK".green().to_string()
            } else {
                "OK".to_string()
            }
        } else if use_color {
            "NOT SET".red().to_string()
        } else {
            "NOT SET".to_string()
        };

        let name_display = if use_color {
            name.bold().to_string()
        } else {
            name.clone()
        };

        table.add_row(vec![name_display, status, caps.join(", ")]);
    }

    println!("{table}");

    let configured_count = providers.iter().filter(|(_, c, _)| *c).count();
    if use_color {
        eprintln!(
            "\n  {}/{} providers configured",
            configured_count.to_string().bold(),
            providers.len()
        );
    } else {
        eprintln!(
            "\n  {}/{} providers configured",
            configured_count,
            providers.len()
        );
    }
    eprintln!();
}

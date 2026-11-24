//! Offline account administration. Only PostgreSQL is contacted.

use std::{
   env,
   ffi::OsString,
   path::PathBuf,
   process::ExitCode,
};

// Share storage and the session format without including the server.
#[allow(dead_code)]
#[path = "../accounts.rs"]
mod accounts;
#[allow(dead_code)]
#[path = "../types/session.rs"]
mod types;

// The server's error type depends on its HTTP error pages. The shared store
// only needs these two variants, both with already-redacted messages.
mod error {
   #[derive(Debug, thiserror::Error)]
   pub enum Error {
      #[error("Configuration error: {0}")]
      InvalidConfig(String),
      #[error("{0}")]
      Internal(String),
   }

   pub type Result<T> = std::result::Result<T, Error>;
}

use accounts::{
   AccountStore,
   Stats,
};
use error::{
   Error,
   Result,
};

const USAGE: &str = "Usage:
  import-accounts migrate
  import-accounts import <file> <search|search-pass|view>
  import-accounts stats
  import-accounts <file> [search|search-pass|view]  (legacy syntax; default: view)

Migration reads TEAPOT_SESSIONS_FILE (default: sessions.jsonl) and
TEAPOT_COOKIES_FILE (default: session-cookies.json).
All commands that access PostgreSQL use TEAPOT_LIMITS_DATABASE_URL.
Import does not validate accounts against X; auth_token is required and ct0 is optional.";

#[derive(Debug, PartialEq, Eq)]
enum Command {
   Help,
   Migrate,
   Import {
      path: PathBuf,
      pool: types::AccountPool,
   },
   Stats,
}

fn parse_pool(pool: &OsString) -> std::result::Result<types::AccountPool, &'static str> {
   pool
      .to_str()
      .and_then(types::AccountPool::parse)
      .ok_or("use search, search-pass, or view for the pool")
}

fn parse_command(args: &[OsString]) -> std::result::Result<Command, &'static str> {
   let import = |path: &OsString, pool| {
      Command::Import {
         path: path.into(),
         pool,
      }
   };
   match args {
      [command] if command == "-h" || command == "--help" => Ok(Command::Help),
      [command] if command == "migrate" => Ok(Command::Migrate),
      [command] if command == "stats" => Ok(Command::Stats),
      [command, path, pool] if command == "import" => Ok(import(path, parse_pool(pool)?)),
      [command, ..] if matches!(command.to_str(), Some("migrate" | "stats" | "import")) => {
         Err("invalid arguments for subcommand")
      },
      [path, ..] if path.to_str().is_some_and(|path| path.starts_with('-')) => {
         Err("unexpected option")
      },
      [path] => Ok(import(path, types::AccountPool::View)),
      [path, pool] => Ok(import(path, parse_pool(pool)?)),
      _ => Err("missing or unexpected arguments"),
   }
}

fn legacy_path(variable: &'static str, default: &str) -> Result<String> {
   match env::var(variable) {
      Ok(path) => Ok(path),
      Err(env::VarError::NotPresent) => Ok(default.to_owned()),
      Err(env::VarError::NotUnicode(_)) => {
         Err(Error::InvalidConfig(format!(
            "{variable} must be valid UTF-8"
         )))
      },
   }
}

fn print_stats(stats: Stats) {
   println!(
      "total={} search={} search_pass={} view={} ct0_dead={} rejected={}",
      stats.total, stats.search, stats.search_pass, stats.view, stats.ct0_dead, stats.rejected
   );
}

async fn run(command: Command) -> Result<()> {
   match command {
      Command::Help => println!("{USAGE}"),
      Command::Migrate => {
         let sessions = legacy_path("TEAPOT_SESSIONS_FILE", "sessions.jsonl")?;
         let cookies = legacy_path("TEAPOT_COOKIES_FILE", "session-cookies.json")?;
         let store = AccountStore::connect().await?;
         store.migrate_legacy(&sessions, &cookies).await?;
         print_stats(store.stats().await?);
      },
      Command::Import { path, pool } => {
         let text = tokio::fs::read_to_string(path)
            .await
            .map_err(|_| Error::Internal("could not read the UTF-8 import file".to_owned()))?;
         let store = AccountStore::connect().await?;
         let summary = store.import_text(&text, pool).await?;
         println!(
            "imported={} duplicates={} invalid={}",
            summary.imported, summary.duplicates, summary.invalid
         );
      },
      Command::Stats => print_stats(AccountStore::connect().await?.stats().await?),
   }
   Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
   let args: Vec<_> = env::args_os().skip(1).collect();
   if let [command, path] = args.as_slice()
      && command == "restore-search"
   {
      let result = async {
         let text = tokio::fs::read_to_string(PathBuf::from(path))
            .await
            .map_err(|_| Error::Internal("could not read the UTF-8 import file".to_owned()))?;
         let store = AccountStore::connect().await?;
         println!("restored={}", store.restore_batch_to_search(&text).await?);
         Ok::<_, Error>(())
      }
      .await;
      return match result {
         Ok(()) => ExitCode::SUCCESS,
         Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
         },
      };
   }
   let command = match parse_command(&args) {
      Ok(command) => command,
      Err(err) => {
         eprintln!("import-accounts: {err}\n\n{USAGE}");
         return ExitCode::from(2);
      },
   };
   // Surface the store's aggregate legacy audit and ignored-jar counts, but
   // do not enable logging from the database driver or use RUST_LOG here.
   tracing_subscriber::fmt()
      .with_env_filter("off,import_accounts::accounts=warn")
      .with_writer(std::io::stderr)
      .without_time()
      .with_target(false)
      .with_ansi(false)
      .init();
   match run(command).await {
      Ok(()) => ExitCode::SUCCESS,
      Err(err) => {
         eprintln!("import-accounts: {err}");
         ExitCode::FAILURE
      },
   }
}

#[cfg(test)]
mod tests {
   use super::*;

   fn parse(args: &[&str]) -> std::result::Result<Command, &'static str> {
      parse_command(&args.iter().map(OsString::from).collect::<Vec<_>>())
   }

   #[test]
   fn recognizes_help_migrate_and_stats() {
      assert_eq!(parse(&["--help"]), Ok(Command::Help));
      assert_eq!(parse(&["-h"]), Ok(Command::Help));
      assert_eq!(parse(&["migrate"]), Ok(Command::Migrate));
      assert_eq!(parse(&["stats"]), Ok(Command::Stats));
   }

   #[test]
   fn routes_explicit_and_legacy_imports_to_the_expected_pool() {
      for (args, pool) in [
         (
            &["import", "input file.txt", "search"][..],
            types::AccountPool::Search,
         ),
         (
            &["import", "input file.txt", "search-pass"][..],
            types::AccountPool::SearchPass,
         ),
         (
            &["import", "input file.txt", "view"][..],
            types::AccountPool::View,
         ),
         (
            &["input file.txt", "search"][..],
            types::AccountPool::Search,
         ),
         (
            &["input file.txt", "search-pass"][..],
            types::AccountPool::SearchPass,
         ),
         (&["input file.txt", "view"][..], types::AccountPool::View),
         (&["input file.txt"][..], types::AccountPool::View),
      ] {
         assert_eq!(
            parse(args),
            Ok(Command::Import {
               path: "input file.txt".into(),
               pool
            })
         );
      }
      assert_eq!(
         parse(&["import", "stats", "view"]),
         Ok(Command::Import {
            path: "stats".into(),
            pool: types::AccountPool::View,
         })
      );
   }

   #[test]
   fn rejects_incomplete_or_surplus_arguments_without_echoing_them() {
      for args in [
         &[][..],
         &["-x"][..],
         &["--help", "extra"][..],
         &["migrate", "extra"][..],
         &["stats", "extra"][..],
         &["import"][..],
         &["import", "input.txt"][..],
         &["import", "input.txt", "search", "extra"][..],
         &["input.txt", "view", "extra"][..],
      ] {
         assert!(parse(args).is_err());
      }
      assert_eq!(
         parse(&["import", "input.txt", "secret"]),
         Err("use search, search-pass, or view for the pool")
      );
   }
}

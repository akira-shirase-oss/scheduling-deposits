//! `booking-server`: load the configuration (before the runtime: it reads secret files),
//! connect, refuse to run as a role or on a database that breaks the protections or the
//! schema contract, serve with every bound, drain.
//!
//! Exit codes: 0 drained cleanly; 1 failed, or the drain deadline cut connections;
//! 2 configuration refused; 3 the role or the protections refused (`booking_db::guard`);
//! 4 the schema is not exactly this binary's (`booking_db::schema`); 5 a business's time
//! zone is not in the zone database (availability would be wrong for it).

use std::process::ExitCode;
use std::sync::Arc;

use booking_accounts::Accounts;
use booking_accounts::mail::{Disabled, MailTransport, SmtpRelay};
use booking_db::guard::{Refusal, check_at_startup};
use booking_db::schema;
use booking_http::stack::ErrorWindow;
use booking_http::{Drained, serve};
use booking_server::config::{Config, LogFormat, Process};
use booking_server::health::Health;
use booking_server::{AppState, app};
use tokio::net::TcpListener;

fn main() -> ExitCode {
    let config = match Config::load(&Process) {
        Ok(config) => config,
        Err(problems) => {
            eprintln!("booking-server: configuration refused:");
            for problem in problems {
                eprintln!("  - {problem}");
            }
            return ExitCode::from(2);
        }
    };
    let Ok(filter) = tracing_subscriber::EnvFilter::try_new(&config.log_filter) else {
        eprintln!("booking-server: configuration refused:\n  - BOOKING_LOG is not a valid filter");
        return ExitCode::from(2);
    };
    match config.log_format {
        LogFormat::Json => tracing_subscriber::fmt()
            .json()
            .with_current_span(true)
            .with_env_filter(filter)
            .init(),
        LogFormat::Text => tracing_subscriber::fmt().with_env_filter(filter).init(),
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            tracing::error!(error = %e, "could not start the runtime");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(config)) {
        Ok(Drained::Clean) => ExitCode::SUCCESS,
        Ok(Drained::DeadlineExceeded { open }) => {
            tracing::warn!(open, "stopped with connections cut at the drain deadline");
            ExitCode::FAILURE
        }
        Err(Stop::Refused(why)) => {
            tracing::error!(reason = %why, "refusing to run");
            ExitCode::from(3)
        }
        Err(Stop::Schema(status)) => {
            tracing::error!(schema = %status, "refusing to run: the schema is not this binary's; see docs/OPERATIONS.md");
            ExitCode::from(4)
        }
        Err(Stop::Zones(zones)) => {
            tracing::error!(zones = %zones, "refusing to run: these time zones are not in the zone database (install tzdata or set TZDIR)");
            ExitCode::from(5)
        }
        Err(Stop::Failed(e)) => {
            tracing::error!(error = %format!("{e:#}"), "stopped");
            ExitCode::FAILURE
        }
    }
}

enum Stop {
    Refused(String),
    Schema(String),
    Zones(String),
    Failed(anyhow::Error),
}

impl<E: Into<anyhow::Error>> From<E> for Stop {
    fn from(e: E) -> Self {
        Self::Failed(e.into())
    }
}

async fn run(config: Config) -> Result<Drained, Stop> {
    let (shutdown, signal) = booking_http::shutdown::on_signal()?;
    let pool = booking_db::connect_lazy(config.database_url.expose(), &config.db)?;
    // The role first; then the schema, since another release's schema also changes what
    // the manifest sees (a newer table, a missing one) and is the cause to report; then the
    // protections.
    let guarded = check_at_startup(&pool, config.startup_timeout).await;
    if let Err(Refusal::Db(e)) = &guarded {
        return Err(Stop::Failed(anyhow::anyhow!(booking_db::chain(e))));
    }
    if matches!(guarded, Ok(()) | Err(Refusal::Unprotected(_))) {
        let status = schema::status(&pool, &booking_db::MIGRATOR).await?;
        if !status.is_current() {
            return Err(Stop::Schema(serde_json::to_string(&status)?));
        }
    }
    if let Err(refused) = guarded {
        return Err(Stop::Refused(refused.to_string()));
    }
    let zones = booking_scheduling::store::zones(&pool)
        .await
        .map_err(|e| Stop::Failed(anyhow::anyhow!("reading the businesses' time zones: {e}")))?;
    let missing: Vec<String> = zones
        .into_iter()
        .filter(|z| booking_scheduling::zone(z).is_err())
        .collect();
    if !missing.is_empty() {
        return Err(Stop::Zones(missing.join(", ")));
    }
    let mail: Arc<dyn MailTransport> = if let Some(relay) = config.smtp_relay {
        Arc::new(SmtpRelay::new(relay)?)
    } else {
        tracing::warn!(
            "BOOKING_SMTP_RELAY is empty: mail is disabled, sign-up reports verification email failed"
        );
        Arc::new(Disabled)
    };
    let accounts = Accounts::new(pool.clone(), config.accounts.clone(), mail).await?;
    let errors = Arc::new(ErrorWindow::default());
    let health = Health::new(
        pool.clone(),
        config.ready_timeout,
        errors.clone(),
        config.error_rate_permille,
        config.error_min_requests,
    );
    let scheduling = booking_scheduling::Scheduling::with_limits(
        accounts.clone(),
        pool.clone(),
        config.scheduling,
    );
    let router = app(AppState {
        accounts,
        scheduling,
        health,
        errors,
        stack: config.stack,
        max_in_flight: config.max_in_flight,
    });
    let listener = TcpListener::bind(config.listen).await?;
    tracing::info!(listen = %listener.local_addr()?, version = env!("CARGO_PKG_VERSION"), "listening");
    let drained = serve(listener, router, config.connection, shutdown).await;
    tracing::warn!(
        target: "booking_server::timing",
        report = %booking_http::timing::global().report(),
        "server timings"
    );
    if let Ok(name) = signal.await {
        tracing::info!(signal = name, "drained after signal");
    }
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), pool.close()).await;
    Ok(drained)
}

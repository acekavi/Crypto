use bot::config::Profile;
use bot::envfile::load_env_file;
use bot::pairs_backtest::{
    audit_current_neighborhood, audit_walk_forward_top_candidates, render_parameter_audit_text,
    render_promotion_gate_text, render_walk_forward_audit_text, run_backtest_with_experiment,
    split_summary_with_experiment, BacktestExperiment,
};
use bot::pairs_config::load_pairs_config;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let profile = Profile::from_name(&args.next().unwrap_or_else(|| "testnet".into()))?;
    load_env_file("/home/acekavi/Projects/Crypto/.env")?;
    let mut bot_id = None::<String>;
    let mut json = false;
    let mut audit_current = false;
    let mut audit_walk_forward = false;
    let mut audit_limit = 5usize;
    let mut min_holdout_trades = 8usize;
    let mut wf_in_sample_days = 180i64;
    let mut wf_out_of_sample_days = 60i64;
    let mut split_pct = 0.70f64;
    let mut experiment = None::<BacktestExperiment>;
    let mut add_on_trigger_r = 1.0f64;
    let mut add_on_fraction = 0.5f64;
    let mut trailing_arm_r = 1.0f64;
    let mut trailing_giveback_r = 1.0f64;
    let mut db_path = "/home/acekavi/Projects/Crypto/data/history.db".to_string();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bot-id" => bot_id = args.next(),
            "--json" => json = true,
            "--audit-current" => audit_current = true,
            "--audit-walk-forward" => audit_walk_forward = true,
            "--audit-limit" => audit_limit = args.next().unwrap_or_else(|| "5".into()).parse()?,
            "--min-holdout-trades" => {
                min_holdout_trades = args.next().unwrap_or_else(|| "8".into()).parse()?
            }
            "--wf-in-sample-days" => {
                wf_in_sample_days = args.next().unwrap_or_else(|| "180".into()).parse()?
            }
            "--wf-out-of-sample-days" => {
                wf_out_of_sample_days = args.next().unwrap_or_else(|| "60".into()).parse()?
            }
            "--split-pct" => split_pct = args.next().unwrap_or_else(|| "0.70".into()).parse()?,
            "--add-on-trigger-r" => add_on_trigger_r = args.next().unwrap_or_else(|| "1.0".into()).parse()?,
            "--add-on-fraction" => add_on_fraction = args.next().unwrap_or_else(|| "0.5".into()).parse()?,
            "--trailing-arm-r" => trailing_arm_r = args.next().unwrap_or_else(|| "1.0".into()).parse()?,
            "--trailing-giveback-r" => trailing_giveback_r = args.next().unwrap_or_else(|| "1.0".into()).parse()?,
            "--experiment" => {
                experiment = Some(match args.next().as_deref() {
                    Some("trailing-stop") => BacktestExperiment::TrailingStop {
                        arm_r: trailing_arm_r,
                        giveback_r: trailing_giveback_r,
                    },
                    Some("add-on") => BacktestExperiment::AddOn {
                        trigger_r: add_on_trigger_r,
                        add_on_fraction,
                    },
                    Some(other) => return Err(format!("unknown experiment {other}").into()),
                    None => return Err("--experiment requires trailing-stop or add-on".into()),
                })
            }
            "--db" => db_path = args.next().unwrap_or(db_path),
            other => return Err(format!("unknown arg {other}").into()),
        }
    }
    let cfg = load_pairs_config(profile)?;
    let bot = bot_id
        .as_deref()
        .map(|id| cfg.bots.iter().find(|b| b.id == id))
        .unwrap_or_else(|| cfg.bots.first())
        .ok_or("no bots configured")?;
    if audit_walk_forward {
        let report = audit_walk_forward_top_candidates(
            &db_path,
            &bot.params,
            split_pct,
            min_holdout_trades,
            audit_limit,
            wf_in_sample_days * 86_400_000,
            wf_out_of_sample_days * 86_400_000,
        )
        .await?;
        if json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            let scoreboard = report
                .scoreboard
                .iter()
                .map(|row| {
                    (
                        row.label.clone(),
                        row.folds_selected,
                        row.oos_profit_factor,
                        row.oos_fixed_profit_factor,
                    )
                })
                .collect::<Vec<_>>();
            let audit_text =
                render_walk_forward_audit_text(&bot.name, report.candidate_pool_size, &report.folds, &scoreboard);
            let gate_text = render_promotion_gate_text(&bot.name, &report.decisions);
            println!("{}\n\n{}", audit_text, gate_text);
        }
    } else if audit_current {
        let report = audit_current_neighborhood(&db_path, &bot.params, split_pct, min_holdout_trades).await?;
        if json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            println!(
                "{}",
                render_parameter_audit_text(&bot.name, &report.candidates, min_holdout_trades, audit_limit)
            );
        }
    } else {
        let summary = split_summary_with_experiment(&db_path, &bot.params, split_pct, experiment).await?;
        if json {
            println!("{}", serde_json::to_string_pretty(&summary)?);
        } else {
            let full = run_backtest_with_experiment(&db_path, &bot.params, None, None, experiment).await?;
            println!(
                "pair={} experiment={} risk_sized trades={} win_rate={:.2}% pf={:.3} net={:.4} dd={:.2}% | fixed_{} full_pf={:.3} full_net={:.4} hold_pf={:.3} hold_net={:.4}",
                full.pair,
                experiment.map(|e| e.as_str()).unwrap_or("baseline"),
                full.trades,
                full.win_rate * 100.0,
                full.profit_factor,
                full.net,
                full.max_drawdown_pct,
                summary.fixed_notional_usdt,
                summary.fixed_notional_full.profit_factor,
                summary.fixed_notional_full.net,
                summary.fixed_notional_holdout.profit_factor,
                summary.fixed_notional_holdout.net,
            );
        }
    }
    Ok(())
}

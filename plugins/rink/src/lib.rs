use abi_stable::std_types::{ROption, RString, RVec};
use anyrun_plugin::*;
use rink_core::{ast, date, gnu_units, CURRENCY_FILE};

#[init]
fn init(_config_dir: RString) -> rink_core::Context {
    let mut ctx = rink_core::Context::new();

    let units = gnu_units::parse_str(rink_core::DEFAULT_FILE.unwrap());
    let dates = date::parse_datefile(rink_core::DATES_FILE);

    let mut currency_defs = Vec::new();

    match reqwest::blocking::get("https://rinkcalc.app/data/currency.json") {
        Ok(response) => match response.json::<ast::Defs>() {
            Ok(mut live_defs) => {
                currency_defs.append(&mut live_defs.defs);
            }
            Err(why) => println!("Error parsing currency json: {}", why),
        },
        Err(why) => println!("Error fetching up-to-date currency conversions: {}", why),
    }

    currency_defs.append(&mut gnu_units::parse_str(CURRENCY_FILE).defs);

    ctx.load(units);
    ctx.load(ast::Defs {
        defs: currency_defs,
    });
    ctx.load_dates(dates);

    ctx
}

#[info]
fn info() -> PluginInfo {
    PluginInfo {
        name: "Rink".into(),
        icon: "accessories-calculator".into(),
    }
}

#[get_matches]
fn get_matches(input: RString, ctx: &mut rink_core::Context) -> RVec<Match> {
    match rink_core::one_line(ctx, &input) {
        Ok(result) => vec![Match {
            title: result.into(),
            description: ROption::RNone,
            use_pango: false,
            icon: ROption::RSome("applications-education-mathematics".into()),
            id: ROption::RNone,
        }]
        .into(),
        Err(_) => RVec::new(),
    }
}

#[handler]
fn handler(selection: Match) -> HandleResult {
    HandleResult::Copy(selection.title.into_bytes())
}

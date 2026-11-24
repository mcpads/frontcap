//! Audit one generation's sequential external-surface receipts; no target is contacted.
use frontcap_core::native::flutter::{Observation, Operation, Request, Response};
use serde::Deserialize;
use std::{collections::HashMap, fs::File};
#[derive(Deserialize)]
struct Exchange {
    request: Request,
    result: Response,
    status: u16,
}
#[derive(Deserialize)]
struct Report {
    exchanges: Vec<Exchange>,
}
fn main() {
    for path in std::env::args().skip(1) {
        let report: Report = serde_json::from_reader(File::open(&path).unwrap()).unwrap();
        let mut time = 0;
        let mut basis: Option<Observation> = None;
        let mut results = HashMap::new();
        let mut accepted = 0;
        let mut generation = None;
        for exchange in report.exchanges {
            if exchange.status != 200 {
                assert!(matches!(exchange.result, Response::Rejected { .. }));
                continue;
            }
            let Response::Completed(result) = exchange.result else {
                panic!("unexpected pending")
            };
            if let Some(previous) = &generation {
                assert_eq!(
                    previous, &exchange.request.generation,
                    "one generation per report"
                );
            } else {
                generation = Some(exchange.request.generation.clone());
            }
            if let Some((request, previous)) = results.get(&exchange.request.id) {
                assert_eq!(request, &exchange.request);
                assert_eq!(previous, &result);
                continue;
            }
            exchange.request.validate_basis(basis.as_ref()).unwrap();
            result
                .validate(&exchange.request, time, basis.as_ref())
                .unwrap();
            time = result.logical_us;
            match &exchange.request.operation {
                Operation::Observe {} => basis = result.surface.clone(),
                Operation::Tap { .. }
                | Operation::Scroll { .. }
                | Operation::ReplaceText { .. }
                | Operation::Advance { .. } => basis = None,
                _ => {}
            }
            if result.not_applied.is_some() {
                basis = None;
            }
            results.insert(exchange.request.id.clone(), (exchange.request, result));
            accepted += 1;
        }
        println!("{path}: {accepted} unique receipts validated, final logical time {time}");
    }
}

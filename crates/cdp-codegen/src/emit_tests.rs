use crate::generate;

const MINI: &str = r#"{"domains":[
 {"domain":"Network","types":[{"id":"TimeSinceEpoch","type":"number"}]},
 {"domain":"Emulation","types":[
   {"id":"VirtualTimePolicy","type":"string","enum":["advance","pause","pauseIfNetworkFetchesPending"]}],
  "commands":[{"name":"setVirtualTimePolicy",
    "parameters":[{"name":"policy","$ref":"VirtualTimePolicy"},
                  {"name":"budget","type":"number","optional":true},
                  {"name":"initialVirtualTime","$ref":"Network.TimeSinceEpoch","optional":true}],
    "returns":[{"name":"virtualTimeTicksBase","type":"number"}]},
   {"name":"clearDeviceMetricsOverride"}],
  "events":[{"name":"virtualTimeBudgetExpired"}]},
 {"domain":"Input","commands":[{"name":"dispatchMouseEvent","parameters":[
    {"name":"type","type":"string","enum":["mousePressed","mouseReleased"]},
    {"name":"x","type":"number"},{"name":"y","type":"number"},
    {"name":"modifiers","type":"integer","optional":true}]}]}
]}"#;

fn file() -> syn::File {
    syn::parse_file(&generate(MINI).unwrap()).unwrap()
}

fn find_struct<'a>(items: &'a [syn::Item], name: &str) -> Option<&'a syn::ItemStruct> {
    items.iter().find_map(|i| match i {
        syn::Item::Struct(s) if s.ident == name => Some(s),
        _ => None,
    })
}

fn module<'a>(f: &'a syn::File, name: &str) -> &'a [syn::Item] {
    f.items
        .iter()
        .find_map(|i| match i {
            syn::Item::Mod(m) if m.ident == name => m.content.as_ref().map(|(_, items)| items.as_slice()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("module {name} missing"))
}

#[test]
fn generates_params_returns_and_event() {
    let f = file();
    let emu = module(&f, "emulation");
    assert!(find_struct(emu, "SetVirtualTimePolicyParams").is_some());
    assert!(find_struct(emu, "SetVirtualTimePolicyReturns").is_some());
    assert!(find_struct(emu, "ClearDeviceMetricsOverrideParams").is_some());
    assert!(find_struct(emu, "VirtualTimeBudgetExpiredEvent").is_some());
    let src = generate(MINI).unwrap();
    assert!(src.contains("const METHOD: &'static str = \"Emulation.setVirtualTimePolicy\""));
    assert!(src.contains("const NAME: &'static str = \"Emulation.virtualTimeBudgetExpired\""));
}

#[test]
fn cross_domain_reference_uses_super_path() {
    let src = generate(MINI).unwrap();
    assert!(src.contains("super::network::TimeSinceEpoch"));
}

#[test]
fn inline_enum_and_constructor() {
    let src = generate(MINI).unwrap();
    assert!(src.contains("pub enum DispatchMouseEventParamsType"));
    assert!(src.contains("pub fn new(r#type: DispatchMouseEventParamsType, x: f64, y: f64) -> Self"));
}

#[test]
fn optional_fields_skip_when_none() {
    let src = generate(MINI).unwrap();
    assert!(src.contains("skip_serializing_if = \"Option::is_none\""));
    assert!(src.contains("pub budget: Option<f64>"));
}

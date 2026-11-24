use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Protocol {
    pub domains: Vec<Domain>,
}

#[derive(Debug, Deserialize)]
pub struct Domain {
    pub domain: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub types: Vec<TypeDef>,
    #[serde(default)]
    pub commands: Vec<Command>,
    #[serde(default)]
    pub events: Vec<Event>,
}

#[derive(Debug, Deserialize)]
pub struct TypeDef {
    pub id: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, rename = "enum")]
    pub enum_values: Option<Vec<String>>,
    #[serde(default)]
    pub properties: Option<Vec<Property>>,
    #[serde(default)]
    pub items: Option<Items>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Property {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default, rename = "$ref")]
    pub reference: Option<String>,
    #[serde(default, rename = "enum")]
    pub enum_values: Option<Vec<String>>,
    #[serde(default)]
    pub items: Option<Items>,
    #[serde(default)]
    pub optional: bool,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Items {
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default, rename = "$ref")]
    pub reference: Option<String>,
    #[serde(default, rename = "enum")]
    pub enum_values: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct Command {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub parameters: Vec<Property>,
    #[serde(default)]
    pub returns: Vec<Property>,
}

#[derive(Debug, Deserialize)]
pub struct Event {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub parameters: Vec<Property>,
}

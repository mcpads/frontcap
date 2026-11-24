//! Original cold-entry evidence, read without acquiring or dispatching a target.
use super::*;
use frontcap_core::native::ios_host::Entry;
fn optional<T:serde::de::DeserializeOwned>(root:&Path,path:&Path) -> io::Result<Option<T>> {
    epochs::unlinked(root,path)?;load(path)
}
pub(super) fn begun(directory:&Path,config:&Config,id:&RequestId,binding:&Binding) -> io::Result<Option<Entry>> {
    let Operation::BeginEntry {previous,url}=&binding.operation else {return Err(bad("not an entry begin"));};
    if !config.cold_entry() || previous!=&binding.owner {return Err(bad("cold entry unavailable or owner differs"));}
    let root=directory.join("host-epochs").join(id.as_str());
    epochs::unlinked(directory,&root.join("app"))?;
    let Some(acceptance)=crate::simulator_app::entry::read_begin(&root.join("app"),&directory.join("target/simulator"),previous,url)? else {return Ok(None);};
    let entry=Entry {begin:id.clone(),acceptance};entry.validate().map_err(bad)?;
    if optional::<Entry>(directory,&root.join("entry.json"))?.is_some_and(|saved|saved!=entry) {return Err(bad("cold entry identity differs"));}
    Ok(Some(entry))
}
pub(super) fn polled(directory:&Path,config:&Config,id:&RequestId,binding:&Binding,begin:&RequestId) -> io::Result<Option<Completion>> {
    let original:Binding=epochs::read(directory,&directory.join("host-requests").join(begin.as_str()).join("request.json"))?;
    if original.owner!=binding.owner || original.epoch!=binding.epoch {return Err(bad("cold poll differs from begin epoch"));}
    if begun(directory,config,begin,&original)?.is_none() {return Ok(None);}
    let root=directory.join("host-epochs").join(begin.as_str());
    let published=optional::<RequestId>(directory,&root.join("entry-published-by.json"))?;
    let saved=optional::<Completion>(directory,&root.join("polls").join(format!("{id}.json")))?;
    if let Some(Completion::EntryPolled {begin:original,ready:None})=&saved {
        if original!=begin || published.as_ref()==Some(id) {return Err(bad("waiting entry poll has replacement publication"));}
        return Ok(saved);
    }
    if published.as_ref()!=Some(id) {
        return if saved.is_some() {Err(bad("ready entry poll has no original publication"))} else {Ok(None)};
    }
    let initial=required(&directory.join("host-ready.json"))?;
    let Some(epoch)=epochs::resolve(directory,config,&initial,Some(begin))? else {return Ok(None);};
    let actual=Completion::EntryPolled {begin:begin.clone(),ready:Some(Box::new(epoch.ready))};
    if saved.is_some_and(|saved|saved!=actual) {return Err(bad("entry poll differs from original replacement"));}
    Ok(Some(actual))
}

#[cfg(test)]
#[path="ios_host_entry_results_tests.rs"]
mod tests;

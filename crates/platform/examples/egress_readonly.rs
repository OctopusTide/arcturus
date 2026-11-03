//! Manual read-only smoke check. Never run by automated tests. One public HTTP request.
use daemon::sys::network::ProxyRoute;
use std::sync::Arc;
fn main() {
    let sys=platform::sys();
    let network=sys.system.network();
    let watch=network.watch(Arc::new(|_| {}));
    println!("network subscriptions: {}",if watch.is_ok(){"ok"}else{"unavailable"});
    if let Err(e)=&watch { println!("listener error: {}",e.message); }
    match network.dns() {Ok(d)=>println!("DNS route assessment: {}",d.state),Err(e)=>println!("DNS assessment unavailable: {}",e.message)}
    match network.get("https://www.cloudflare.com/cdn-cgi/trace",&ProxyRoute::System) {
        Ok(body)=>println!("Cloudflare country: {}",body.lines().find_map(|l|l.strip_prefix("loc=")).unwrap_or("missing")),
        Err(e)=>println!("Cloudflare request unavailable: {}",e.message),
    }
    drop(watch);
}

//! The pages the gateway serves itself, to browsers: the starting page and the error pages.
//!
//! Each is one self-contained HTML document (no external CSS, scripts or images), since the app behind it isn't
//! there to serve anything.

/// The request header the starting page polls with. The gateway answers such a request itself, with
/// `{"state": …}`, and never passes it to the app.
pub const STATUS_HEADER: &str = "kiwi-nest-status";

const STYLE: &str = "\
:root{color-scheme:light dark;--fg:#1d2327;--muted:#5f6b72;--bg:#f6f7f8;--accent:#2f7d5b}\
@media (prefers-color-scheme:dark){:root{--fg:#e7eaec;--muted:#9aa5ab;--bg:#15191c;--accent:#5cc094}}\
*{box-sizing:border-box}\
body{margin:0;min-height:100vh;display:grid;place-items:center;background:var(--bg);color:var(--fg);\
font:16px/1.5 system-ui,-apple-system,\"Segoe UI\",Roboto,sans-serif}\
main{max-width:28rem;padding:2rem;text-align:center}\
h1{font-size:1.35rem;margin:1rem 0 .25rem}\
p{margin:.25rem 0;color:var(--muted)}\
a{color:var(--accent)}\
.spin{width:2.25rem;height:2.25rem;margin:0 auto;border-radius:50%;border:.25rem solid color-mix(in srgb,var(--accent) 25%,transparent);\
border-top-color:var(--accent);animation:s 0.9s linear infinite}\
.spin.off{animation:none;border-color:var(--muted)}\
@keyframes s{to{transform:rotate(360deg)}}\
@media (prefers-reduced-motion:reduce){.spin{animation-duration:3s}}";

/// Polls the gateway once a second. It reloads only once the app is ready; a failure or anything unexpected shows
/// a message and stops, so a failed start never turns into a reload loop. It gives up after ten minutes.
const SCRIPT: &str = r#"(function(){
var t0=Date.now(),done=false;
function show(title,text){done=true;document.getElementById("spin").className="spin off";
document.getElementById("title").textContent=title;document.getElementById("text").innerHTML=text;}
function fail(){show("This site couldn't start","Something went wrong while starting it. <a href=\"\">Try again</a> in a little while.");}
function later(){if(done)return;if(Date.now()-t0>600000){show("This site is taking too long to start","<a href=\"\">Try again</a> in a little while.");return;}setTimeout(poll,1000);}
function poll(){
fetch(location.href,{headers:{"Kiwi-Nest-Status":"1"},cache:"no-store",credentials:"same-origin",redirect:"error"})
.then(function(r){return r.json();})
.then(function(s){
if(s.state==="ready"){done=true;location.reload();}
else if(s.state==="starting"){later();}
else if(s.state==="suspended"){show("This site is suspended","It has been switched off for now.");}
else{fail();}
},later);}
setTimeout(poll,1000);
})();"#;

fn document(title: &str, body: &str, script: Option<&str>) -> String {
    let script = script.map(|s| format!("<script>{s}</script>")).unwrap_or_default();
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <meta name=\"robots\" content=\"noindex\"><title>{title}</title><style>{STYLE}</style></head>\
         <body><main>{body}</main>{script}</body></html>"
    )
}

/// The page a browser gets while the app starts.
pub fn starting() -> String {
    document(
        "Starting…",
        "<div id=\"spin\" class=\"spin\" role=\"status\" aria-label=\"Starting\"></div>\
         <h1 id=\"title\">Starting up</h1>\
         <p id=\"text\">This site was asleep. It'll be ready in a moment.</p>",
        Some(SCRIPT),
    )
}

/// A page saying why the app can't be reached. It never reloads itself.
pub fn message(title: &str, text: &str) -> String {
    document(
        title,
        &format!("<div class=\"spin off\"></div><h1>{title}</h1><p>{text}</p>"),
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_starting_page_is_self_contained_and_reloads_only_when_ready() {
        let page = starting();
        for external in ["src=", "href=\"http", "@import", "url("] {
            assert!(!page.contains(external), "{external} in the page");
        }
        assert_eq!(page.matches("location.reload()").count(), 1);
        assert!(page.contains(r#"if(s.state==="ready"){done=true;location.reload();}"#));
        assert!(page.contains("Kiwi-Nest-Status"));
        assert!(!message("x", "y").contains("<script"));
    }
}

//! Pure JavaScript source generators injected into CEF frames.
//!
//! Relocated verbatim from the parent module; every function returns a
//! `String`/`&'static str` script payload and performs no FFI. Parent FFI
//! wrappers and the test suite reach these via the parent's `use scripts::*`.

use super::*;

pub(super) fn cosmetic_filter_script(css: &str) -> String {
    let css = js_string_literal(css);
    format!(
        "(function(){{var id='mde-cef-cosmetic-style';var root=document.head||document.documentElement;if(!root)return;var el=document.getElementById(id);if({css}.length===0){{if(el)el.remove();return;}}if(!el){{el=document.createElement('style');el.id=id;root.appendChild(el);}}el.textContent={css};}})();"
    )
}

/// Fill the page's first login form with a user-chosen saved credential: locate the
/// first `input[type=password]`, then (within its form) the best-guess username field,
/// set both values, and dispatch `input`+`change` so the page's own JS observes the
/// edit. User-initiated only (never auto-runs on load); credentials are the
/// shell's session-only store. Unquoted CSS attribute selectors keep the embedded
/// JS free of nested quotes.
pub(super) fn login_fill_script(username: &str, password: &str) -> String {
    let user = js_string_literal(username);
    let pass = js_string_literal(password);
    format!(
        "(function(){{var pw=document.querySelector('input[type=password]');if(!pw)return;var scope=pw.form||document;var u=scope.querySelector('input[autocomplete=username],input[type=email],input[name*=user i],input[name*=email i],input[id*=user i],input[type=text]');function set(el,val){{if(!el)return;try{{el.focus();}}catch(e){{}}el.value=val;el.dispatchEvent(new Event('input',{{bubbles:true}}));el.dispatchEvent(new Event('change',{{bubbles:true}}));}}set(u,{user});set(pw,{pass});}})();"
    )
}

/// Page-side login-capture bridge (password-manager auto-capture): a capture-phase
/// `submit` listener beacons username/password for any form carrying a non-empty
/// password field to the login-capture URL, which the engine intercepts + cancels
/// before the network. The engine validates the separate `origin` query value
/// against CEF's cached top-level URL before the shell sees the event. Idempotent
/// per context.
pub(super) fn login_capture_script() -> String {
    format!(
        "(function(){{if(window.__mdeLoginCaptureInstalled)return;window.__mdeLoginCaptureInstalled=true;document.addEventListener('submit',function(e){{try{{var form=e.target;if(!form||!form.querySelector)return;var pw=form.querySelector('input[type=password]');if(!pw||!pw.value)return;var u=form.querySelector('input[autocomplete=username],input[type=email],input[name*=user i],input[name*=email i],input[id*=user i],input[type=text]');var body=JSON.stringify({{username:u?u.value:'',password:pw.value}});fetch('{prefix}?origin='+encodeURIComponent(location.origin)+'&body='+encodeURIComponent(body),{{mode:'no-cors',keepalive:true}}).catch(function(){{}});}}catch(_e){{}}}},true);}})();",
        prefix = CEF_LOGIN_BEACON_PREFIX
    )
}

pub(super) fn force_dark_script(enabled: bool) -> String {
    if !enabled {
        return "(function(){var id='mde-cef-force-dark-style';var el=document.getElementById(id);if(el)el.remove();document.documentElement.style.colorScheme='';})();".to_owned();
    }
    let css = r#"
:root { color-scheme: dark !important; background: #0f1419 !important; }
html, body { background: #0f1419 !important; color: #f2f4f8 !important; }
body, main, article, section, nav, aside, header, footer, div {
  background-color: color-mix(in srgb, currentColor 0%, #0f1419 100%) !important;
}
p, span, li, td, th, label, input, textarea, select, button, a, h1, h2, h3, h4, h5, h6 {
  color: #f2f4f8 !important;
}
a { color: #78a9ff !important; }
img, video, canvas, picture, svg, iframe { filter: none !important; }
input, textarea, select, button { background: #202830 !important; border-color: #525c66 !important; }
"#;
    let css = js_string_literal(css);
    format!(
        "(function(){{var id='mde-cef-force-dark-style';var root=document.head||document.documentElement;if(!root)return;var el=document.getElementById(id);if(!el){{el=document.createElement('style');el.id=id;root.appendChild(el);}}document.documentElement.style.colorScheme='dark';el.textContent={css};}})();"
    )
}

pub(super) fn reader_mode_script(enabled: bool) -> String {
    if !enabled {
        return "(function(){var id='mde-cef-reader-style';var el=document.getElementById(id);if(el)el.remove();document.documentElement.classList.remove('mde-reader-mode');})();".to_owned();
    }
    let css = r#"
html.mde-reader-mode body {
  max-width: 76ch !important;
  margin: 0 auto !important;
  padding: 3rem 2rem !important;
  line-height: 1.65 !important;
  font-size: 18px !important;
  font-family: Inter, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif !important;
}
html.mde-reader-mode article, html.mde-reader-mode main {
  max-width: 76ch !important;
  margin-left: auto !important;
  margin-right: auto !important;
}
html.mde-reader-mode nav, html.mde-reader-mode aside, html.mde-reader-mode footer,
html.mde-reader-mode [role="navigation"], html.mde-reader-mode [aria-label*="advert"],
html.mde-reader-mode iframe, html.mde-reader-mode embed {
  display: none !important;
}
html.mde-reader-mode p, html.mde-reader-mode li {
  margin-block: 0.85em !important;
}
html.mde-reader-mode img, html.mde-reader-mode video {
  max-width: 100% !important;
  height: auto !important;
}
"#;
    let css = js_string_literal(css);
    format!(
        "(function(){{var id='mde-cef-reader-style';var root=document.head||document.documentElement;if(!root)return;var el=document.getElementById(id);if(!el){{el=document.createElement('style');el.id=id;root.appendChild(el);}}el.textContent={css};document.documentElement.classList.add('mde-reader-mode');}})();"
    )
}

pub(super) fn autoplay_block_script(blocked: bool) -> String {
    if !blocked {
        return r#"(function(){var s=window.__mdeAutoplayBlocker;if(s){try{if(s.observer)s.observer.disconnect();}catch(_e){}try{document.removeEventListener('pointerdown',s.allow,true);document.removeEventListener('keydown',s.allow,true);document.removeEventListener('touchstart',s.allow,true);document.removeEventListener('click',s.allow,true);}catch(_e){}try{if(s.originalPlay&&s.patchedPlay&&window.HTMLMediaElement&&HTMLMediaElement.prototype.play===s.patchedPlay){HTMLMediaElement.prototype.play=s.originalPlay;}}catch(_e){}}delete window.__mdeAutoplayBlocker;delete document.documentElement.dataset.mdeAutoplayBlocked;})();"#.to_owned();
    }
    r#"(function(){var root=document.documentElement;if(!root)return;root.dataset.mdeAutoplayBlocked='true';var s=window.__mdeAutoplayBlocker;if(s&&s.sweep){s.sweep(document);return;}s={allowed:false};window.__mdeAutoplayBlocker=s;s.allow=function(e){if(e&&e.isTrusted===false)return;s.allowed=true;};document.addEventListener('pointerdown',s.allow,true);document.addEventListener('keydown',s.allow,true);document.addEventListener('touchstart',s.allow,true);document.addEventListener('click',s.allow,true);s.blockedError=function(){try{return new DOMException('Autoplay blocked by MDE Browser','NotAllowedError');}catch(_e){var err=new Error('Autoplay blocked by MDE Browser');err.name='NotAllowedError';return err;}};s.sweep=function(scope){try{var base=scope&&scope.querySelectorAll?scope:document;var media=base.querySelectorAll('audio[autoplay],video[autoplay]');for(var i=0;i<media.length;i++){var el=media[i];if(s.allowed||el.dataset.mdeAutoplayAllowed==='true')continue;el.autoplay=false;el.removeAttribute('autoplay');try{el.pause();}catch(_e){}}}catch(_e){}};try{var proto=window.HTMLMediaElement&&HTMLMediaElement.prototype;if(proto&&proto.play&&!s.originalPlay){s.originalPlay=proto.play;s.patchedPlay=function(){if(s.allowed||this.dataset.mdeAutoplayAllowed==='true'||!document.documentElement.dataset.mdeAutoplayBlocked){return s.originalPlay.apply(this,arguments);}try{this.pause();}catch(_e){}return Promise.reject(s.blockedError());};try{Object.defineProperty(proto,'play',{value:s.patchedPlay,writable:true,configurable:true});}catch(_e){proto.play=s.patchedPlay;}}}catch(_e){}if(window.MutationObserver){s.observer=new MutationObserver(function(records){for(var i=0;i<records.length;i++){for(var j=0;j<records[i].addedNodes.length;j++){var n=records[i].addedNodes[j];if(n&&n.nodeType===1)s.sweep(n);}}});s.observer.observe(document.documentElement,{childList:true,subtree:true});}s.sweep(document);})();"#.to_owned()
}

pub(super) const fn media_playback_toggle_script() -> &'static str {
    r#"(function(){try{var list=[].slice.call(document.querySelectorAll('audio,video')).filter(function(el){return !el.ended&&(el.readyState>0||el.currentSrc||el.src);});if(!list.length)return;var playing=list.find(function(el){return !el.paused&&!el.ended;});if(playing){for(var i=0;i<list.length;i++){if(!list[i].paused&&!list[i].ended){try{list[i].pause();}catch(_e){}}}return;}var target=list.find(function(el){return el.paused&&!el.ended;})||list[0];try{target.dataset.mdeAutoplayAllowed='true';}catch(_e){}try{var p=target.play();if(p&&p.catch)p.catch(function(){});}catch(_e){}}catch(_e){}})();"#
}

pub(super) fn media_transport_script(action: MediaTransportAction) -> String {
    let action = match action {
        MediaTransportAction::PlayPause => "playPause",
        MediaTransportAction::Play => "play",
        MediaTransportAction::Pause => "pause",
        MediaTransportAction::Stop => "stop",
        MediaTransportAction::Next => "next",
        MediaTransportAction::Previous => "previous",
    };
    format!(
        r#"(function(){{try{{var action='{action}';var list=[].slice.call(document.querySelectorAll('audio,video')).filter(function(el){{return !el.ended&&(el.readyState>0||el.currentSrc||el.src||el.currentTime>0);}});if(!list.length)return;var playing=list.find(function(el){{return !el.paused&&!el.ended;}});var current=playing||list.find(function(el){{return el.currentTime>0&&!el.ended;}})||list[0];function play(el){{if(!el)return;try{{el.dataset.mdeAutoplayAllowed='true';}}catch(_e){{}}try{{var p=el.play();if(p&&p.catch)p.catch(function(){{}});}}catch(_e){{}}}}function pause(el){{try{{el.pause();}}catch(_e){{}}}}function seek(el,t){{try{{if(isFinite(t)){{if(el.fastSeek)el.fastSeek(t);else el.currentTime=t;}}}}catch(_e){{}}}}function pauseActive(){{for(var i=0;i<list.length;i++){{if(!list[i].paused&&!list[i].ended)pause(list[i]);}}}}if(action==='pause'){{pauseActive();return;}}if(action==='stop'){{for(var i=0;i<list.length;i++){{pause(list[i]);seek(list[i],0);}}return;}}if(action==='play'){{play(current);return;}}if(action==='playPause'){{if(playing)pauseActive();else play(current);return;}}var dir=action==='next'?1:-1;var wasPlaying=!!playing;var idx=Math.max(0,list.indexOf(current));if(list.length>1){{pause(current);var target=list[(idx+dir+list.length)%list.length];seek(target,0);play(target);return;}}if(action==='next'){{var end=isFinite(current.duration)&&current.duration>0?current.duration:current.currentTime+30;seek(current,end);}}else{{seek(current,0);}}if(wasPlaying)play(current);}}catch(_e){{}}}})();"#
    )
}

pub(super) fn user_agent_override_script(user_agent: &str) -> String {
    if user_agent.trim().is_empty() {
        return "(function(){delete window.__mdeUserAgentOverride;})();".to_owned();
    }
    let ua = js_string_literal(&clamp_utf8(user_agent, 512));
    format!(
        "(function(){{var ua={ua};window.__mdeUserAgentOverride=ua;try{{Object.defineProperty(Navigator.prototype,'userAgent',{{get:function(){{return window.__mdeUserAgentOverride||ua;}},configurable:true}});Object.defineProperty(Navigator.prototype,'appVersion',{{get:function(){{return window.__mdeUserAgentOverride||ua;}},configurable:true}});Object.defineProperty(Navigator.prototype,'platform',{{get:function(){{return /Android|Mobile|iPhone|iPad/.test(window.__mdeUserAgentOverride||ua)?'Linux armv8l':'Linux x86_64';}},configurable:true}});}}catch(_e){{}}}})();"
    )
}

pub(super) fn device_profile_script(
    profile: &str,
    width: u16,
    height: u16,
    scale_percent: u16,
    touch: bool,
) -> String {
    if profile == "default" || width == 0 || height == 0 {
        return "(function(){delete window.__mdeDeviceProfile;try{delete window.innerWidth;delete window.innerHeight;delete window.devicePixelRatio;}catch(_e){}var meta=document.getElementById('mde-device-profile-viewport');if(meta)meta.remove();delete document.documentElement.dataset.mdeDeviceProfile;})();".to_owned();
    }
    let profile = js_string_literal(&clamp_utf8(profile, 32));
    let width = width.clamp(240, 7680);
    let height = height.clamp(240, 7680);
    let scale = scale_percent.clamp(50, 600);
    let touch_points = if touch { 5 } else { 0 };
    format!(
        "(function(){{var p={{profile:{profile},width:{width},height:{height},dpr:{scale}/100,touch:{touch},touchPoints:{touch_points}}};window.__mdeDeviceProfile=p;document.documentElement.dataset.mdeDeviceProfile=p.profile;var meta=document.getElementById('mde-device-profile-viewport');if(!meta){{meta=document.createElement('meta');meta.id='mde-device-profile-viewport';meta.name='viewport';(document.head||document.documentElement).appendChild(meta);}}meta.content='width='+p.width+', initial-scale=1';function def(o,n,g){{try{{Object.defineProperty(o,n,{{get:g,configurable:true}});}}catch(_e){{}}}}def(window,'innerWidth',function(){{return p.width;}});def(window,'innerHeight',function(){{return p.height;}});def(window,'devicePixelRatio',function(){{return p.dpr;}});if(window.Screen&&Screen.prototype){{def(Screen.prototype,'width',function(){{return p.width;}});def(Screen.prototype,'height',function(){{return p.height;}});def(Screen.prototype,'availWidth',function(){{return p.width;}});def(Screen.prototype,'availHeight',function(){{return p.height;}});}}if(window.Navigator&&Navigator.prototype){{def(Navigator.prototype,'maxTouchPoints',function(){{return p.touchPoints;}});}}}})();"
    )
}

pub(super) fn userscript_library_script(enabled: bool, bundle: &str) -> String {
    if !enabled {
        return "(function(){var style=document.getElementById('mde-browser-userscript-style');if(style)style.remove();if(window.__mdeBrowserUserScriptsObserver){window.__mdeBrowserUserScriptsObserver.disconnect();window.__mdeBrowserUserScriptsObserver=null;}delete document.documentElement.dataset.mdeBrowserUserscripts;})();".to_owned();
    }
    format!(
        "(function(){{try{{document.documentElement.dataset.mdeBrowserUserscripts='true';\n{bundle}\n}}catch(err){{console.warn('mde userscript bundle failed',err);}}}})();"
    )
}

pub(super) fn spellcheck_highlight_script(words: &[String]) -> String {
    let words: Vec<String> = words
        .iter()
        .filter_map(|word| {
            let trimmed = word.trim();
            if trimmed.len() < 2 || trimmed.len() > 64 {
                None
            } else {
                Some(trimmed.to_owned())
            }
        })
        .take(64)
        .collect();
    let words = js_string_array_literal(&words);
    r#"(function(){
var cls='mde-browser-spell-miss';
var old=document.querySelectorAll('span.'+cls);
for(var i=old.length-1;i>=0;i--){var n=old[i];n.replaceWith(document.createTextNode(n.textContent||''));}
if(!document.body){return;}
document.body.normalize();
var words=__WORDS__;
if(!words.length){delete document.documentElement.dataset.mdeBrowserSpellcheck;return;}
var style=document.getElementById('mde-browser-spellcheck-style');
if(!style){style=document.createElement('style');style.id='mde-browser-spellcheck-style';(document.head||document.documentElement).appendChild(style);}
style.textContent='span.'+cls+'{text-decoration: underline wavy #d13438; text-decoration-thickness: 1.5px; text-underline-offset: 0.12em;}';
var escaped=words.map(function(w){return String(w).replace(/[.*+?^${}()|[\]\\]/g,'\\$&');}).filter(Boolean);
if(!escaped.length){return;}
var re=new RegExp('\\b('+escaped.join('|')+')\\b','gi');
var walker=document.createTreeWalker(document.body,NodeFilter.SHOW_TEXT,{acceptNode:function(node){
  var p=node.parentElement;
  if(!p||p.closest('script,style,textarea,input,select,span.'+cls))return NodeFilter.FILTER_REJECT;
  re.lastIndex=0;
  return re.test(node.nodeValue||'')?NodeFilter.FILTER_ACCEPT:NodeFilter.FILTER_REJECT;
}});
var nodes=[];
while(nodes.length<256){var node=walker.nextNode();if(!node)break;nodes.push(node);}
for(var n=0;n<nodes.length;n++){
  var text=nodes[n].nodeValue||'';re.lastIndex=0;
  var frag=document.createDocumentFragment();var last=0;var m;
  while((m=re.exec(text))&&frag.childNodes.length<512){
    if(m.index>last)frag.appendChild(document.createTextNode(text.slice(last,m.index)));
    var span=document.createElement('span');span.className=cls;span.dataset.mdeBrowserSpellcheck='miss';span.textContent=m[0];frag.appendChild(span);
    last=m.index+m[0].length;
  }
  if(last<text.length)frag.appendChild(document.createTextNode(text.slice(last)));
  nodes[n].replaceWith(frag);
}
document.documentElement.dataset.mdeBrowserSpellcheck=String(words.length);
})();"#
        .replace("__WORDS__", &words)
}

pub(super) fn spellcheck_correction_script(word: &str, replacement: &str) -> String {
    spellcheck_correction_script_with_target(word, replacement, Some(0))
}

pub(super) fn spellcheck_correction_all_script(word: &str, replacement: &str) -> String {
    spellcheck_correction_script_with_target(word, replacement, None)
}

pub(super) fn spellcheck_correction_at_script(
    word: &str,
    replacement: &str,
    occurrence: u16,
) -> String {
    spellcheck_correction_script_with_target(word, replacement, Some(occurrence))
}

fn spellcheck_correction_script_with_target(
    word: &str,
    replacement: &str,
    target_occurrence: Option<u16>,
) -> String {
    let word = word.trim();
    let replacement = replacement.trim();
    if word.is_empty() || replacement.is_empty() || word.len() > 64 || replacement.len() > 128 {
        return "()=>{}".to_owned();
    }
    let word = js_string_literal(word);
    let replacement = js_string_literal(replacement);
    let target_occurrence = target_occurrence.map_or(-1, i32::from);
    format!(
        r#"(function(){{
var word={word};
var replacement={replacement};
var targetOccurrence={target_occurrence};
var replaceAll=targetOccurrence<0;
var cls='mde-browser-spell-miss';
function same(value){{return String(value||'').toLocaleLowerCase()===word.toLocaleLowerCase();}}
var marks=document.querySelectorAll('span.'+cls);
var changed=0;var seen=0;var markMatches=0;
for(var i=0;i<marks.length;i++){{
  if(same(marks[i].textContent)){{
    markMatches++;
    if(!replaceAll&&seen!==targetOccurrence){{seen++;continue;}}
    marks[i].replaceWith(document.createTextNode(replacement));
    changed++;
    if(!replaceAll){{
      document.body&&document.body.normalize();
      return;
    }}
    seen++;
  }}
}}
if(markMatches>0&&!replaceAll)return;
if(changed>0){{
  document.body&&document.body.normalize();
  return;
}}
if(!document.body)return;
var escaped=word.replace(/[.*+?^${{}}()|[\]\\]/g,'\\$&');
var re=new RegExp('\\b'+escaped+'\\b','gi');
var walker=document.createTreeWalker(document.body,NodeFilter.SHOW_TEXT,{{acceptNode:function(node){{
  var p=node.parentElement;
  if(!p||p.closest('script,style,textarea,input,select'))return NodeFilter.FILTER_REJECT;
  re.lastIndex=0;
  return re.test(node.nodeValue||'')?NodeFilter.FILTER_ACCEPT:NodeFilter.FILTER_REJECT;
}}}});
var node;var total=0;
while((node=walker.nextNode())&&total<512){{
  var text=node.nodeValue||'';
  re.lastIndex=0;
  if(!replaceAll){{
    var m;
    while((m=re.exec(text))&&total<512){{
      if(total===targetOccurrence){{
        node.nodeValue=text.slice(0,m.index)+replacement+text.slice(m.index+m[0].length);
        return;
      }}
      total++;
    }}
    continue;
  }}
  var next=text.replace(re,function(m){{total++;return total<=512?replacement:m;}});
  if(next!==text)node.nodeValue=next;
}}
}})();"#
    )
}

pub(super) fn page_text_beacon_script(id: u64, max_bytes: u32) -> String {
    let max_bytes = max_bytes.clamp(1, CEF_PAGE_TEXT_BEACON_MAX_BYTES);
    format!(
        "(function(){{try{{var cap={max_bytes};var root=document.body||document.documentElement;\
var text=root?String(root.innerText||root.textContent||''):'';\
text=text.replace(/\\s+/g,' ').trim();if(text.length>cap)text=text.slice(0,cap);\
var img=document.createElement('img');img.alt='';img.width=1;img.height=1;\
img.style.cssText='position:absolute;left:-9999px;top:-9999px;width:1px;height:1px';\
img.src='{}{}?text='+encodeURIComponent(text);\
(document.body||document.documentElement).appendChild(img);}}catch(err){{\
var fallback=document.createElement('img');fallback.alt='';fallback.width=1;fallback.height=1;\
fallback.style.cssText='position:absolute;left:-9999px;top:-9999px;width:1px;height:1px';\
fallback.src='{}{}?text=';(document.body||document.documentElement).appendChild(fallback);}}}})();",
        CEF_PAGE_TEXT_BEACON_PREFIX, id, CEF_PAGE_TEXT_BEACON_PREFIX, id
    )
}

pub(super) fn page_scrape_beacon_script(
    id: u64,
    max_bytes: u32,
    max_links: u16,
    max_headings: u16,
) -> String {
    let max_bytes = max_bytes.clamp(1, CEF_PAGE_SCRAPE_BEACON_MAX_BYTES as u32);
    let max_links = max_links.min(128);
    let max_headings = max_headings.min(64);
    format!(
        r#"(function(){{try{{
var textCap=Math.min({max_bytes},16384),linkCap={max_links},headingCap={max_headings},articleCap=8192,bodyCap={body_cap};
function trim(v,n){{v=String(v||'').replace(/\s+/g,' ').trim();return v.length>n?v.slice(0,n):v;}}
function visible(el){{try{{if(!el||!el.getClientRects||!el.getClientRects().length)return false;var s=getComputedStyle(el);return s.visibility!=='hidden'&&s.display!=='none';}}catch(_){{return true;}}}}
var root=document.body||document.documentElement;
var raw=root?String(root.innerText||root.textContent||''):'';
var normalized=trim(raw,textCap);
var articleNode=null,articleSelector='';
var candidates=document.querySelectorAll?document.querySelectorAll('article,main,[role=main]'):[];
for(var c=0;c<candidates.length;c++){{if(visible(candidates[c])){{articleNode=candidates[c];articleSelector=(articleNode.tagName||'').toLowerCase();if(articleNode.getAttribute&&articleNode.getAttribute('role'))articleSelector+='[role='+articleNode.getAttribute('role')+']';break;}}}}
var articleRaw=articleNode?String(articleNode.innerText||articleNode.textContent||''):'';
var articleText=trim(articleRaw,articleCap);
var links=[];
var anchors=document.querySelectorAll?document.querySelectorAll('a[href]'):[];
for(var i=0;i<anchors.length&&links.length<linkCap;i++){{var a=anchors[i];if(!visible(a))continue;var href=trim(a.href||a.getAttribute('href')||'',2048);if(!href)continue;links.push({{url:href,text:trim(a.innerText||a.textContent||a.getAttribute('aria-label')||'',160),rel:trim(a.getAttribute('rel')||'',80),target:trim(a.getAttribute('target')||'',40)}});}}
var headings=[];
var hs=document.querySelectorAll?document.querySelectorAll('h1,h2,h3,h4,h5,h6'):[];
for(var h=0;h<hs.length&&headings.length<headingCap;h++){{var el=hs[h];if(!visible(el))continue;var label=trim(el.innerText||el.textContent||'',240);if(!label)continue;headings.push({{level:Number(String(el.tagName||'H0').slice(1))||0,text:label}});}}
var canonicalEl=document.querySelector?document.querySelector('link[rel~="canonical"][href]'):null;
var descriptionEl=document.querySelector?document.querySelector('meta[name="description" i][content],meta[property="og:description"][content]'):null;
function payload(){{return {{text:normalized,text_truncated:trim(raw,2147483647).length>textCap,article_text:articleText,article_text_truncated:trim(articleRaw,2147483647).length>articleCap,article_selector:articleSelector,canonical_url:canonicalEl?trim(canonicalEl.href||canonicalEl.getAttribute('href')||'',2048):'',meta_description:descriptionEl?trim(descriptionEl.getAttribute('content')||'',512):'',document_lang:trim((document.documentElement&&document.documentElement.lang)||'',64),links:links,headings:headings}};}}
var body=JSON.stringify(payload());
if(body.length>bodyCap){{links=links.slice(0,32);headings=headings.slice(0,16);normalized=trim(normalized,8192);articleText=trim(articleText,4096);body=JSON.stringify(payload());}}
if(body.length>bodyCap){{links=[];headings=[];normalized=trim(normalized,4096);articleText=trim(articleText,2048);body=JSON.stringify(payload());}}
var img=document.createElement('img');img.alt='';img.width=1;img.height=1;img.style.cssText='position:absolute;left:-9999px;top:-9999px;width:1px;height:1px';img.src='{prefix}{id}?body='+encodeURIComponent(body);(document.body||document.documentElement).appendChild(img);
}}catch(err){{var fallback=document.createElement('img');fallback.alt='';fallback.width=1;fallback.height=1;fallback.style.cssText='position:absolute;left:-9999px;top:-9999px;width:1px;height:1px';fallback.src='{prefix}{id}?body=';(document.body||document.documentElement).appendChild(fallback);}}}})();"#,
        body_cap = CEF_PAGE_SCRAPE_BEACON_MAX_BYTES,
        prefix = CEF_PAGE_SCRAPE_BEACON_PREFIX,
    )
}

/// Best-effort renderer-level removal of the JS-reachable WebRTC surface.
///
/// `chromium_privacy_switches()` (`cef_init.rs`) cannot fully disable WebRTC
/// at the command-line level: `--disable-webrtc` is not a real Chromium
/// switch (verified against the live `content_switches.cc`/
/// `chrome_switches.cc` upstream — Chromium silently no-ops unrecognized `--`
/// switches rather than erroring), and the only genuine kill switch is the
/// build-time GN flag `enable_webrtc=false`, unavailable on this crate's
/// vendored prebuilt CEF binary. This deletes the JS constructors/entry
/// points instead, matching the CEF community's own recommended technique
/// (remove the interfaces from the renderer's global scope at script-inject
/// time) and this codebase's existing shim-injection pattern
/// (`passkey_bridge_script`). Injected once per navigation generation (and
/// re-applied through a fresh document's settle window — see `ShimInjector` /
/// `inject_context_shims`) rather than on a blind 250 ms timer; the installed
/// `MutationObserver` keeps late subframes covered within a stable document.
/// This is a baseline privacy default, not a per-tab, user-toggleable feature.
///
/// This is defense-in-depth, not an airtight guarantee: this ABI has no
/// `OnContextCreated`-equivalent early-injection hook, so a page's own inline
/// script can still run before the first injection lands, and a fresh
/// document commit (e.g. an in-page navigation) gets an unpatched JS context
/// until the navigation-driven re-injection re-applies this. `--force-webrtc-ip
/// -handling-policy=disable_non_proxied_udp` (kept in
/// `chromium_privacy_switches()`, verified real) is the second layer: even a
/// same-tick `RTCPeerConnection` that gets past this script still cannot leak a
/// raw local IP over non-proxied UDP.
///
/// Cross-engine posture (browser-5). The shell runs CEF and Servo
/// interchangeably on the same seat (CEF is the default when its runtime is
/// present; the `mde-web-preview` Servo helper is the fallback), so a given
/// user's WebRTC guarantee depends on which engine rendered the page. The two
/// engines are deliberately brought as close as each platform allows:
/// * Servo turns WebRTC off at the engine level — `secure_preferences()` sets
///   `dom_webrtc_enabled = false`, so `RTCPeerConnection` never exists and there
///   is no bypass. This is the *reference* posture; it must not be weakened.
/// * CEF has no equivalent hard off switch on a prebuilt binary (see the
///   `chromium_privacy_switches()` doc), so it reaches parity for the *actual
///   harm* — the raw-local-IP leak — via the engine-level
///   `--force-webrtc-ip-handling-policy` switch above, and removes the JS API
///   surface only best-effort via this shim.
///
/// Residual gap flagged for the operator (CEF-only, and minimized, not closed):
/// because this ABI has no early `OnContextCreated` hook, a hostile page's own
/// inline script can touch the WebRTC *API surface* (e.g. construct an
/// `RTCPeerConnection`) in the sub-tick before this shim's first injection lands.
/// That surviving connection still cannot leak a raw local IP — the engine-level
/// ip-handling switch blocks non-proxied UDP regardless — so the residual is API
/// *presence*, not the IP leak Servo defends against. Fully closing it needs
/// either the build-time `enable_webrtc=false` GN flag (unavailable on the
/// vendored CEF) or a native `CefPermissionHandler` deny (no ABI vtable offset
/// verified from the pinned CEF 149 headers). Revisit if either becomes
/// available.
pub(super) const fn webrtc_block_script() -> &'static str {
    // browser-3: the removal is applied to EVERY reachable frame, not just the
    // main frame. `strip(w)` deletes the JS-reachable WebRTC surface on a target
    // window; `sweep(w)` recurses through `w.frames` so a child (or nested)
    // same-origin iframe — the trivial main-frame-only bypass — is covered too.
    // A `MutationObserver` re-sweeps on DOM mutation so a *newly inserted* iframe
    // is patched as soon as it appears, between the 250ms poll ticks. Cross-origin
    // subframes are unreachable from JS by same-origin policy (property access on
    // them throws and is swallowed) — the `--force-webrtc-ip-handling-policy`
    // switch remains the backstop for that residual, see this file's cef_init
    // companion. A native `CefPermissionHandler`/ICE-layer deny would be airtight
    // but the pinned CEF 149 ABI exposes no permission-handler or frame-enumeration
    // vtable offset verified from the farm headers, so it is not attempted here.
    "(function(){function strip(w){try{delete w.RTCPeerConnection;}catch(_e){}try{delete w.webkitRTCPeerConnection;}catch(_e){}try{delete w.RTCDataChannel;}catch(_e){}try{delete w.RTCSessionDescription;}catch(_e){}try{delete w.RTCIceCandidate;}catch(_e){}try{if(w.MediaDevices&&w.MediaDevices.prototype){delete w.MediaDevices.prototype.getUserMedia;delete w.MediaDevices.prototype.getDisplayMedia;}}catch(_e){}try{if(w.navigator&&w.navigator.mediaDevices){delete w.navigator.mediaDevices.getUserMedia;delete w.navigator.mediaDevices.getDisplayMedia;}}catch(_e){}try{delete w.navigator.getUserMedia;}catch(_e){}try{delete w.navigator.webkitGetUserMedia;}catch(_e){}try{delete w.navigator.mozGetUserMedia;}catch(_e){}}function sweep(w){try{strip(w);}catch(_e){}var kids=null;try{kids=w.frames;}catch(_e){kids=null;}if(kids){for(var i=0;i<kids.length;i++){var cw=null;try{cw=kids[i];}catch(_e){cw=null;}if(cw&&cw!==w){try{sweep(cw);}catch(_e){}}}}}sweep(window);try{if(!window.__mdeWebrtcBlockObserver&&window.MutationObserver&&document&&document.documentElement){window.__mdeWebrtcBlockObserver=new MutationObserver(function(){try{sweep(window);}catch(_e){}});window.__mdeWebrtcBlockObserver.observe(document.documentElement,{childList:true,subtree:true});}}catch(_e){}})();"
}

/// Install the page WebAuthn/passkey interception bridge (browser-5 parity note).
///
/// This is NOT a CEF-only capability: the Servo helper ships an equivalent bridge
/// (`mde-web-preview::engine::poll_passkey_request` / `passkey_bridge_drain_script`),
/// and both route ceremonies to the same daemon-owned passkey worker. Passkey
/// posture is therefore at parity across the two interchangeable engines — keep it
/// that way when either bridge changes.
pub(super) fn passkey_bridge_script() -> String {
    format!(
        r#"(function(){{
try{{
  if(!window.__mdeBrowserPasskeyQueue)window.__mdeBrowserPasskeyQueue=[];
  if(!window.__mdeBrowserPasskeyPending)window.__mdeBrowserPasskeyPending={{}};
  if(!window.__mdeBrowserPasskeyComplete){{
    window.__mdeBrowserPasskeyComplete=function(event){{
      try{{
        event=event||{{}};
        var id=String(event.client_request_id||'');
        var pending=window.__mdeBrowserPasskeyPending&&window.__mdeBrowserPasskeyPending[id];
        if(!pending)return false;
        delete window.__mdeBrowserPasskeyPending[id];
        function ab(v){{
          try{{
            v=String(v||'').replace(/-/g,'+').replace(/_/g,'/');
            while(v.length%4)v+='=';
            var s=atob(v),out=new Uint8Array(s.length);
            for(var i=0;i<s.length;i++)out[i]=s.charCodeAt(i);
            return out.buffer;
          }}catch(_){{return new ArrayBuffer(0);}}
        }}
        if(event.error||event.state==='error'){{
          pending.reject(new DOMException(String(event.error||'Passkey ceremony failed'),'NotAllowedError'));
          return true;
        }}
        function setProto(obj,ctor){{try{{if(ctor&&ctor.prototype)Object.setPrototypeOf(obj,ctor.prototype);}}catch(_){{}}return obj;}}
        function b64(v){{return String(v||'');}}
        var credentialId=String(event.credential_id_b64url||'');
        var response={{}};
        if(event.op==='browser_passkey_assertion'||event.ceremony==='get'){{
          response.authenticatorData=ab(event.authenticator_data_b64url);
          response.clientDataJSON=ab(event.client_data_json_b64url);
          response.signature=ab(event.signature_b64url);
          response.userHandle=ab(event.user_handle_b64url);
          response.toJSON=function(){{return {{authenticatorData:b64(event.authenticator_data_b64url),clientDataJSON:b64(event.client_data_json_b64url),signature:b64(event.signature_b64url),userHandle:b64(event.user_handle_b64url)}};}};
          setProto(response,window.AuthenticatorAssertionResponse);
        }}else{{
          response.clientDataJSON=ab(event.client_data_json_b64url);
          response.attestationObject=ab(event.attestation_object_b64url);
          response.getPublicKey=function(){{return ab(event.public_key_spki_der_b64url||event.public_key_sec1_b64url);}};
          response.getPublicKeyAlgorithm=function(){{return Number(event.cose_alg||-7);}};
          response.getTransports=function(){{return ['internal'];}};
          response.getAuthenticatorData=function(){{return ab(event.authenticator_data_b64url);}};
          response.toJSON=function(){{return {{attestationObject:b64(event.attestation_object_b64url),clientDataJSON:b64(event.client_data_json_b64url),publicKey:b64(event.public_key_spki_der_b64url||event.public_key_sec1_b64url),publicKeyAlgorithm:Number(event.cose_alg||-7),authenticatorData:b64(event.authenticator_data_b64url),transports:['internal']}};}};
          setProto(response,window.AuthenticatorAttestationResponse);
        }}
        var credential={{id:credentialId,rawId:ab(credentialId),type:'public-key',authenticatorAttachment:'platform',response:response}};
        credential.getClientExtensionResults=function(){{return {{}};}};
        credential.toJSON=function(){{return {{id:credentialId,rawId:credentialId,type:'public-key',authenticatorAttachment:'platform',response:response.toJSON?response.toJSON():{{}},clientExtensionResults:{{}}}};}};
        pending.resolve(setProto(credential,window.PublicKeyCredential));
        return true;
      }}catch(err){{return false;}}
    }};
  }}
  function emit(item){{
    try{{
      var img=document.createElement('img');img.alt='';img.width=1;img.height=1;
      img.style.cssText='position:absolute;left:-9999px;top:-9999px;width:1px;height:1px';
      img.src='{prefix}?body='+encodeURIComponent(JSON.stringify(item).slice(0,8192));
      (document.body||document.documentElement).appendChild(img);
    }}catch(_){{}}
  }}
  if(!window.__mdeBrowserPasskeyBridgeInstalled){{
    window.__mdeBrowserPasskeyBridgeInstalled=true;
    window.__mdeBrowserPasskeySeq=window.__mdeBrowserPasskeySeq||0;
    try{{
      if(!window.PublicKeyCredential)window.PublicKeyCredential=function PublicKeyCredential(){{}};
      if(!window.PublicKeyCredential.isUserVerifyingPlatformAuthenticatorAvailable)window.PublicKeyCredential.isUserVerifyingPlatformAuthenticatorAvailable=function(){{return Promise.resolve(true);}};
      if(!window.PublicKeyCredential.isConditionalMediationAvailable)window.PublicKeyCredential.isConditionalMediationAvailable=function(){{return Promise.resolve(false);}};
      if(!window.AuthenticatorAttestationResponse)window.AuthenticatorAttestationResponse=function AuthenticatorAttestationResponse(){{}};
      if(!window.AuthenticatorAssertionResponse)window.AuthenticatorAssertionResponse=function AuthenticatorAssertionResponse(){{}};
    }}catch(_){{}}
    function trim(v,n){{v=String(v||'').trim();return v.length>n?v.slice(0,n):v;}}
    function b64url(value){{
      try{{
        if(value==null)return '';
        if(typeof value==='string')return value.replace(/=+$/,'').replace(/\+/g,'-').replace(/\//g,'_');
        var bytes=null;
        if(value instanceof ArrayBuffer)bytes=new Uint8Array(value);
        else if(ArrayBuffer.isView(value))bytes=new Uint8Array(value.buffer,value.byteOffset,value.byteLength);
        if(!bytes)return '';
        var s='',max=Math.min(bytes.length,1536);
        for(var i=0;i<max;i++)s+=String.fromCharCode(bytes[i]);
        return btoa(s).replace(/=+$/,'').replace(/\+/g,'-').replace(/\//g,'_');
      }}catch(_){{return '';}}
    }}
    function hasUserGesture(){{
      try{{
        var ua=navigator.userActivation;
        if(ua&&typeof ua.isActive==='boolean')return ua.isActive;
      }}catch(_){{}}
      return false;
    }}
    function ceremony(kind,options){{
      var pk=(options&&options.publicKey)||{{}};
      var rp=(pk.rp&&pk.rp.id)||location.hostname;
      var out={{ceremony:kind,origin:String(location.href||''),rp_id:trim(rp,253),challenge_b64url:b64url(pk.challenge)}};
      if(kind==='create'&&pk.user){{
        out.user_handle_b64url=b64url(pk.user.id);
        out.user_name=trim(pk.user.displayName||pk.user.name||'',256);
      }}
      if(kind==='get'&&Array.isArray(pk.allowCredentials)){{
        out.allow_credentials=pk.allowCredentials.slice(0,64).map(function(c){{return b64url(c&&c.id);}}).filter(Boolean);
      }}
      if(typeof pk.timeout==='number')out.timeout_ms=Math.max(0,Math.floor(pk.timeout));
      out.user_present=hasUserGesture();
      return out;
    }}
    function enqueue(kind,options){{
      var item=ceremony(kind,options);
      if(!item.challenge_b64url)return Promise.reject(new DOMException('Passkey challenge missing','NotAllowedError'));
      if(!item.user_present)return Promise.reject(new DOMException('Passkey ceremony requires a user gesture','NotAllowedError'));
      item.client_request_id='mde-pk-'+Date.now().toString(36)+'-'+(++window.__mdeBrowserPasskeySeq).toString(36);
      var q=window.__mdeBrowserPasskeyQueue;
      q.push(item);
      while(q.length>16)q.shift();
      return new Promise(function(resolve,reject){{window.__mdeBrowserPasskeyPending[item.client_request_id]={{resolve:resolve,reject:reject,ceremony:kind}};}});
    }}
    var creds=navigator.credentials||(navigator.credentials={{}});
    var origCreate=(typeof creds.create==='function')?creds.create.bind(creds):null;
    var origGet=(typeof creds.get==='function')?creds.get.bind(creds):null;
    creds.create=function(options){{
      if(options&&options.publicKey)return enqueue('create',options);
      if(origCreate)return origCreate(options);
      return Promise.reject(new DOMException('Unsupported credential type','NotSupportedError'));
    }};
    creds.get=function(options){{
      if(options&&options.publicKey)return enqueue('get',options);
      if(origGet)return origGet(options);
      return Promise.reject(new DOMException('Unsupported credential type','NotSupportedError'));
    }};
    window.__mdeBrowserPasskeyDrain=function(){{try{{var dq=window.__mdeBrowserPasskeyQueue;if(dq){{for(var n=0;n<4&&dq.length;n++)emit(dq.shift());}}}}catch(_){{}}}};
  }}
  var q=window.__mdeBrowserPasskeyQueue;
  for(var n=0;n<4&&q.length;n++)emit(q.shift());
}}catch(_){{}}
}})();"#,
        prefix = CEF_PASSKEY_BEACON_PREFIX
    )
}

pub(super) fn passkey_complete_script(body: &str) -> String {
    let body = js_string_literal(body);
    format!(
        "(function(){{try{{var event=JSON.parse({body});if(window.__mdeBrowserPasskeyComplete)window.__mdeBrowserPasskeyComplete(event);}}catch(_){{}}}})();"
    )
}

/// browser-8: the lightweight passkey heartbeat. It only calls the drain closure
/// installed once by [`passkey_bridge_script`], so the multi-KB bridge shim is no
/// longer recompiled/re-executed every 250 ms — but page-initiated ceremonies are
/// still delivered promptly. A no-op until the bridge has been installed for the
/// current document (`__mdeBrowserPasskeyDrain` undefined), which is exactly when
/// there is nothing to drain.
pub(super) const fn passkey_drain_script() -> &'static str {
    "(function(){try{if(window.__mdeBrowserPasskeyDrain)window.__mdeBrowserPasskeyDrain();}catch(_){}})();"
}

//! The curated built-in userscript library — a fixed table of per-site CSS
//! "fixups" (declutter/reader/quiet rules for ~100 well-known hosts) plus the
//! `curated_userscript_bundle()` builder that renders them into the injectable JS
//! the Browser helper runs to apply the matching host's rule set. Self-contained
//! data + one pure string builder using `serde_json` directly. A pure relocation
//! from the `web` god-module.

#[derive(Clone, Copy)]
pub(super) struct CuratedUserscriptRule {
    id: &'static str,
    hosts: &'static [&'static str],
    css: &'static str,
}

const NEWS_CLEANUP_CSS: &str = r#"[class*="newsletter" i],[id*="newsletter" i],[class*="paywall" i],[id*="paywall" i],[class*="modal" i],[id*="modal" i],[class*="overlay" i],[id*="overlay" i],[class*="advert" i],[id*="advert" i],[class*="sponsor" i],[id*="sponsor" i]{display:none!important;}main,article{line-height:1.62!important;}article{max-width:82ch!important;margin-inline:auto!important;}"#;
const VIDEO_FOCUS_CSS: &str = r#"[class*="ad" i],[id*="ad" i],[class*="promo" i],[id*="promo" i],[class*="recommended" i],[id*="recommended" i],[class*="shorts" i],[id*="shorts" i]{display:none!important;}video{max-height:82vh!important;}"#;
const COMMERCE_CLEANUP_CSS: &str = r#"[class*="sponsored" i],[id*="sponsored" i],[class*="recommend" i],[id*="recommend" i],[class*="upsell" i],[id*="upsell" i],[class*="newsletter" i],[id*="newsletter" i]{display:none!important;}main{scroll-margin-top:1rem!important;}"#;
const DOCS_READABLE_CSS: &str = r#"[class*="survey" i],[id*="survey" i],[class*="feedback" i],[id*="feedback" i],[class*="toc" i] nav[aria-label*="secondary" i]{display:none!important;}article,main{max-width:92ch!important;line-height:1.58!important;}pre{white-space:pre-wrap!important;}"#;
const SOCIAL_QUIET_CSS: &str = r#"[aria-label*="trend" i],[data-testid*="trend" i],[class*="promoted" i],[data-testid*="promoted" i],[class*="suggest" i],[aria-label*="suggest" i],[class*="ad" i]{display:none!important;}main{max-width:74ch!important;margin-inline:auto!important;}"#;
const MUSIC_QUIET_CSS: &str = r#"[class*="ad" i],[id*="ad" i],[class*="sponsor" i],[id*="sponsor" i],[data-testid*="upgrade" i],[aria-label*="upgrade" i]{display:none!important;}main{background:#121212!important;}"#;
const RECIPE_CLEANUP_CSS: &str = r#"[class*="jump" i],[class*="video" i],[class*="newsletter" i],[class*="ad" i],[id*="ad" i],[class*="sponsor" i],[class*="related" i]{display:none!important;}article,main{max-width:78ch!important;margin-inline:auto!important;line-height:1.62!important;}"#;

pub(super) static CURATED_USERSCRIPTS: &[CuratedUserscriptRule] = &[
    CuratedUserscriptRule { id: "youtube-focus", hosts: &["youtube.com", "youtu.be"], css: "ytd-rich-section-renderer,ytd-reel-shelf-renderer,#masthead-ad,ytd-promoted-sparkles-web-renderer{display:none!important;}#secondary{max-width:360px!important;}" },
    CuratedUserscriptRule { id: "npr-reader", hosts: &["npr.org"], css: ".bucketwrap,.sponsor,.ad,.advertisement,[data-metrics*=\"sponsor\"]{display:none!important;}article{max-width:76ch!important;margin-inline:auto!important;line-height:1.65!important;}article img{max-width:100%!important;height:auto!important;}" },
    CuratedUserscriptRule { id: "spotify-quiet", hosts: &["open.spotify.com"], css: "[data-testid=\"upgrade-button\"],.Root__right-sidebar,[aria-label*=\"Sponsored\"]{display:none!important;}.main-view-container{background:#121212!important;}" },
    CuratedUserscriptRule { id: "wikipedia-readable", hosts: &["wikipedia.org", "wikimedia.org"], css: ".vector-page-titlebar-toc,.vector-column-start,.vector-toc-landmark{display:none!important;}.mw-body{max-width:92ch!important;margin-inline:auto!important;line-height:1.62!important;}" },
    CuratedUserscriptRule { id: "nytimes-clean-reader", hosts: &["nytimes.com"], css: NEWS_CLEANUP_CSS },
    CuratedUserscriptRule { id: "washingtonpost-clean-reader", hosts: &["washingtonpost.com"], css: NEWS_CLEANUP_CSS },
    CuratedUserscriptRule { id: "guardian-clean-reader", hosts: &["theguardian.com"], css: NEWS_CLEANUP_CSS },
    CuratedUserscriptRule { id: "cnn-clean-reader", hosts: &["cnn.com"], css: NEWS_CLEANUP_CSS },
    CuratedUserscriptRule { id: "apnews-clean-reader", hosts: &["apnews.com"], css: NEWS_CLEANUP_CSS },
    CuratedUserscriptRule { id: "reuters-clean-reader", hosts: &["reuters.com"], css: NEWS_CLEANUP_CSS },
    CuratedUserscriptRule { id: "bbc-clean-reader", hosts: &["bbc.com", "bbc.co.uk"], css: NEWS_CLEANUP_CSS },
    CuratedUserscriptRule { id: "theatlantic-clean-reader", hosts: &["theatlantic.com"], css: NEWS_CLEANUP_CSS },
    CuratedUserscriptRule { id: "wired-clean-reader", hosts: &["wired.com"], css: NEWS_CLEANUP_CSS },
    CuratedUserscriptRule { id: "arstechnica-clean-reader", hosts: &["arstechnica.com"], css: NEWS_CLEANUP_CSS },
    CuratedUserscriptRule { id: "techcrunch-clean-reader", hosts: &["techcrunch.com"], css: NEWS_CLEANUP_CSS },
    CuratedUserscriptRule { id: "theverge-clean-reader", hosts: &["theverge.com"], css: NEWS_CLEANUP_CSS },
    CuratedUserscriptRule { id: "vox-clean-reader", hosts: &["vox.com"], css: NEWS_CLEANUP_CSS },
    CuratedUserscriptRule { id: "propublica-clean-reader", hosts: &["propublica.org"], css: NEWS_CLEANUP_CSS },
    CuratedUserscriptRule { id: "politico-clean-reader", hosts: &["politico.com"], css: NEWS_CLEANUP_CSS },
    CuratedUserscriptRule { id: "axios-clean-reader", hosts: &["axios.com"], css: NEWS_CLEANUP_CSS },
    CuratedUserscriptRule { id: "bloomberg-clean-reader", hosts: &["bloomberg.com"], css: NEWS_CLEANUP_CSS },
    CuratedUserscriptRule { id: "wsj-clean-reader", hosts: &["wsj.com"], css: NEWS_CLEANUP_CSS },
    CuratedUserscriptRule { id: "ft-clean-reader", hosts: &["ft.com"], css: NEWS_CLEANUP_CSS },
    CuratedUserscriptRule { id: "economist-clean-reader", hosts: &["economist.com"], css: NEWS_CLEANUP_CSS },
    CuratedUserscriptRule { id: "latimes-clean-reader", hosts: &["latimes.com"], css: NEWS_CLEANUP_CSS },
    CuratedUserscriptRule { id: "usatoday-clean-reader", hosts: &["usatoday.com"], css: NEWS_CLEANUP_CSS },
    CuratedUserscriptRule { id: "nbcnews-clean-reader", hosts: &["nbcnews.com"], css: NEWS_CLEANUP_CSS },
    CuratedUserscriptRule { id: "cbsnews-clean-reader", hosts: &["cbsnews.com"], css: NEWS_CLEANUP_CSS },
    CuratedUserscriptRule { id: "abcnews-clean-reader", hosts: &["abcnews.go.com"], css: NEWS_CLEANUP_CSS },
    CuratedUserscriptRule { id: "pbs-clean-reader", hosts: &["pbs.org"], css: NEWS_CLEANUP_CSS },
    CuratedUserscriptRule { id: "youtube-music-quiet", hosts: &["music.youtube.com"], css: MUSIC_QUIET_CSS },
    CuratedUserscriptRule { id: "soundcloud-quiet", hosts: &["soundcloud.com"], css: MUSIC_QUIET_CSS },
    CuratedUserscriptRule { id: "bandcamp-quiet", hosts: &["bandcamp.com"], css: MUSIC_QUIET_CSS },
    CuratedUserscriptRule { id: "tidal-quiet", hosts: &["tidal.com"], css: MUSIC_QUIET_CSS },
    CuratedUserscriptRule { id: "pandora-quiet", hosts: &["pandora.com"], css: MUSIC_QUIET_CSS },
    CuratedUserscriptRule { id: "deezer-quiet", hosts: &["deezer.com"], css: MUSIC_QUIET_CSS },
    CuratedUserscriptRule { id: "apple-music-quiet", hosts: &["music.apple.com"], css: MUSIC_QUIET_CSS },
    CuratedUserscriptRule { id: "vimeo-focus", hosts: &["vimeo.com"], css: VIDEO_FOCUS_CSS },
    CuratedUserscriptRule { id: "twitch-focus", hosts: &["twitch.tv"], css: VIDEO_FOCUS_CSS },
    CuratedUserscriptRule { id: "dailymotion-focus", hosts: &["dailymotion.com"], css: VIDEO_FOCUS_CSS },
    CuratedUserscriptRule { id: "netflix-focus", hosts: &["netflix.com"], css: VIDEO_FOCUS_CSS },
    CuratedUserscriptRule { id: "hulu-focus", hosts: &["hulu.com"], css: VIDEO_FOCUS_CSS },
    CuratedUserscriptRule { id: "disneyplus-focus", hosts: &["disneyplus.com"], css: VIDEO_FOCUS_CSS },
    CuratedUserscriptRule { id: "max-focus", hosts: &["max.com"], css: VIDEO_FOCUS_CSS },
    CuratedUserscriptRule { id: "peacock-focus", hosts: &["peacocktv.com"], css: VIDEO_FOCUS_CSS },
    CuratedUserscriptRule { id: "primevideo-focus", hosts: &["primevideo.com"], css: VIDEO_FOCUS_CSS },
    CuratedUserscriptRule { id: "github-readable", hosts: &["github.com"], css: DOCS_READABLE_CSS },
    CuratedUserscriptRule { id: "gitlab-readable", hosts: &["gitlab.com"], css: DOCS_READABLE_CSS },
    CuratedUserscriptRule { id: "stackoverflow-readable", hosts: &["stackoverflow.com", "stackexchange.com"], css: DOCS_READABLE_CSS },
    CuratedUserscriptRule { id: "mdn-readable", hosts: &["developer.mozilla.org"], css: DOCS_READABLE_CSS },
    CuratedUserscriptRule { id: "rust-docs-readable", hosts: &["doc.rust-lang.org", "docs.rs"], css: DOCS_READABLE_CSS },
    CuratedUserscriptRule { id: "python-docs-readable", hosts: &["docs.python.org"], css: DOCS_READABLE_CSS },
    CuratedUserscriptRule { id: "go-docs-readable", hosts: &["go.dev"], css: DOCS_READABLE_CSS },
    CuratedUserscriptRule { id: "kubernetes-docs-readable", hosts: &["kubernetes.io"], css: DOCS_READABLE_CSS },
    CuratedUserscriptRule { id: "docker-docs-readable", hosts: &["docs.docker.com"], css: DOCS_READABLE_CSS },
    CuratedUserscriptRule { id: "archwiki-readable", hosts: &["wiki.archlinux.org"], css: DOCS_READABLE_CSS },
    CuratedUserscriptRule { id: "ubuntu-docs-readable", hosts: &["ubuntu.com"], css: DOCS_READABLE_CSS },
    CuratedUserscriptRule { id: "redhat-docs-readable", hosts: &["access.redhat.com", "docs.redhat.com"], css: DOCS_READABLE_CSS },
    CuratedUserscriptRule { id: "fedora-docs-readable", hosts: &["docs.fedoraproject.org"], css: DOCS_READABLE_CSS },
    CuratedUserscriptRule { id: "aws-docs-readable", hosts: &["docs.aws.amazon.com"], css: DOCS_READABLE_CSS },
    CuratedUserscriptRule { id: "gcp-docs-readable", hosts: &["cloud.google.com"], css: DOCS_READABLE_CSS },
    CuratedUserscriptRule { id: "azure-docs-readable", hosts: &["learn.microsoft.com"], css: DOCS_READABLE_CSS },
    CuratedUserscriptRule { id: "openai-docs-readable", hosts: &["platform.openai.com"], css: DOCS_READABLE_CSS },
    CuratedUserscriptRule { id: "anthropic-docs-readable", hosts: &["docs.anthropic.com"], css: DOCS_READABLE_CSS },
    CuratedUserscriptRule { id: "hackernews-readable", hosts: &["news.ycombinator.com"], css: "tr.athing{font-size:15px!important;}td.subtext{font-size:12px!important;}.pagetop{line-height:1.6!important;}" },
    CuratedUserscriptRule { id: "reddit-quiet", hosts: &["reddit.com"], css: SOCIAL_QUIET_CSS },
    CuratedUserscriptRule { id: "x-quiet", hosts: &["x.com", "twitter.com"], css: SOCIAL_QUIET_CSS },
    CuratedUserscriptRule { id: "facebook-quiet", hosts: &["facebook.com"], css: SOCIAL_QUIET_CSS },
    CuratedUserscriptRule { id: "instagram-quiet", hosts: &["instagram.com"], css: SOCIAL_QUIET_CSS },
    CuratedUserscriptRule { id: "linkedin-quiet", hosts: &["linkedin.com"], css: SOCIAL_QUIET_CSS },
    CuratedUserscriptRule { id: "threads-quiet", hosts: &["threads.net"], css: SOCIAL_QUIET_CSS },
    CuratedUserscriptRule { id: "mastodon-quiet", hosts: &["mastodon.social"], css: SOCIAL_QUIET_CSS },
    CuratedUserscriptRule { id: "bluesky-quiet", hosts: &["bsky.app"], css: SOCIAL_QUIET_CSS },
    CuratedUserscriptRule { id: "amazon-clean-shop", hosts: &["amazon.com"], css: COMMERCE_CLEANUP_CSS },
    CuratedUserscriptRule { id: "ebay-clean-shop", hosts: &["ebay.com"], css: COMMERCE_CLEANUP_CSS },
    CuratedUserscriptRule { id: "etsy-clean-shop", hosts: &["etsy.com"], css: COMMERCE_CLEANUP_CSS },
    CuratedUserscriptRule { id: "walmart-clean-shop", hosts: &["walmart.com"], css: COMMERCE_CLEANUP_CSS },
    CuratedUserscriptRule { id: "target-clean-shop", hosts: &["target.com"], css: COMMERCE_CLEANUP_CSS },
    CuratedUserscriptRule { id: "bestbuy-clean-shop", hosts: &["bestbuy.com"], css: COMMERCE_CLEANUP_CSS },
    CuratedUserscriptRule { id: "newegg-clean-shop", hosts: &["newegg.com"], css: COMMERCE_CLEANUP_CSS },
    CuratedUserscriptRule { id: "homedepot-clean-shop", hosts: &["homedepot.com"], css: COMMERCE_CLEANUP_CSS },
    CuratedUserscriptRule { id: "lowes-clean-shop", hosts: &["lowes.com"], css: COMMERCE_CLEANUP_CSS },
    CuratedUserscriptRule { id: "costco-clean-shop", hosts: &["costco.com"], css: COMMERCE_CLEANUP_CSS },
    CuratedUserscriptRule { id: "allrecipes-clean-recipe", hosts: &["allrecipes.com"], css: RECIPE_CLEANUP_CSS },
    CuratedUserscriptRule { id: "seriouseats-clean-recipe", hosts: &["seriouseats.com"], css: RECIPE_CLEANUP_CSS },
    CuratedUserscriptRule { id: "foodnetwork-clean-recipe", hosts: &["foodnetwork.com"], css: RECIPE_CLEANUP_CSS },
    CuratedUserscriptRule { id: "epicurious-clean-recipe", hosts: &["epicurious.com"], css: RECIPE_CLEANUP_CSS },
    CuratedUserscriptRule { id: "bonappetit-clean-recipe", hosts: &["bonappetit.com"], css: RECIPE_CLEANUP_CSS },
    CuratedUserscriptRule { id: "nyt-cooking-clean-recipe", hosts: &["cooking.nytimes.com"], css: RECIPE_CLEANUP_CSS },
    CuratedUserscriptRule { id: "weather-clean-panel", hosts: &["weather.com", "wunderground.com"], css: NEWS_CLEANUP_CSS },
    CuratedUserscriptRule { id: "imdb-clean-page", hosts: &["imdb.com"], css: COMMERCE_CLEANUP_CSS },
    CuratedUserscriptRule { id: "rottentomatoes-clean-page", hosts: &["rottentomatoes.com"], css: NEWS_CLEANUP_CSS },
    CuratedUserscriptRule { id: "goodreads-clean-page", hosts: &["goodreads.com"], css: COMMERCE_CLEANUP_CSS },
    CuratedUserscriptRule { id: "medium-clean-reader", hosts: &["medium.com"], css: NEWS_CLEANUP_CSS },
    CuratedUserscriptRule { id: "substack-clean-reader", hosts: &["substack.com"], css: NEWS_CLEANUP_CSS },
    CuratedUserscriptRule { id: "quora-clean-reader", hosts: &["quora.com"], css: NEWS_CLEANUP_CSS },
    CuratedUserscriptRule { id: "pinterest-quiet", hosts: &["pinterest.com"], css: SOCIAL_QUIET_CSS },
    CuratedUserscriptRule { id: "tiktok-focus", hosts: &["tiktok.com"], css: VIDEO_FOCUS_CSS },
    CuratedUserscriptRule { id: "coursera-readable", hosts: &["coursera.org"], css: DOCS_READABLE_CSS },
    CuratedUserscriptRule { id: "edx-readable", hosts: &["edx.org"], css: DOCS_READABLE_CSS },
];

/// A user-authored site style — a host the rule applies to and a CSS body — the safe,
/// non-gated slice of "userscripts": CSS injection only (what the curated engine
/// already does), never arbitrary JS (which would be a threat-model change). Rendered
/// alongside [`CURATED_USERSCRIPTS`] by [`curated_userscript_bundle`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct UserSiteStyle {
    pub(super) host: String,
    pub(super) css: String,
}

pub(super) fn curated_userscript_bundle(user_styles: &[UserSiteStyle]) -> String {
    let mut rules = CURATED_USERSCRIPTS
        .iter()
        .map(|rule| {
            serde_json::json!({
                "id": rule.id,
                "hosts": rule.hosts,
                "css": rule.css,
            })
        })
        .collect::<Vec<_>>();
    // User-authored CSS rules render exactly like curated ones (host-matched CSS),
    // with a `user:` id so they're distinguishable; a blank host or CSS is skipped.
    for (i, style) in user_styles.iter().enumerate() {
        let host = style
            .host
            .trim()
            .trim_start_matches("www.")
            .to_ascii_lowercase();
        if host.is_empty() || style.css.trim().is_empty() {
            continue;
        }
        rules.push(serde_json::json!({
            "id": format!("user:{i}"),
            "hosts": [host],
            "css": style.css,
        }));
    }
    let rules_json = serde_json::to_string(&rules).expect("curated userscript rules encode");
    format!(
        r#"(function(){{
var rules={rules_json};
var host=(location.hostname||'').toLowerCase().replace(/^www\./,'');
var active=rules.filter(function(rule){{return rule.hosts.some(function(pattern){{return host===pattern||host.endsWith('.'+pattern);}});}});
var root=document.head||document.documentElement;
if(!root)return;
var style=document.getElementById('mde-browser-userscript-style');
if(!style){{style=document.createElement('style');style.id='mde-browser-userscript-style';root.appendChild(style);}}
style.textContent=active.map(function(rule){{return '/* '+rule.id+' */\n'+rule.css;}}).join('\n');
document.documentElement.dataset.mdeBrowserUserscripts='true';
document.documentElement.dataset.mdeBrowserUserscriptCount=String(active.length);
if(window.__mdeBrowserUserScriptsObserver)window.__mdeBrowserUserScriptsObserver.disconnect();
window.__mdeBrowserUserScriptsObserver=new MutationObserver(function(){{document.documentElement.dataset.mdeBrowserUserscripts='true';}});
window.__mdeBrowserUserScriptsObserver.observe(document.documentElement,{{childList:true,subtree:true}});
}})();"#
    )
}

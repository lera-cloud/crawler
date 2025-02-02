use super::black_list::contains;
use super::configuration::{Configuration, get_ua};
use super::page::{build, get_page_selectors, Page};
use super::robotparser::RobotFileParser;
use super::utils::log;
use crate::rpc::client::{monitor, WebsiteServiceClient};
use case_insensitive_string::CaseInsensitiveString;
use compact_str::CompactString;
use hashbrown::HashSet;
use reqwest::header::CONNECTION;
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::Client;
use sitemap::{
    reader::{SiteMapEntity, SiteMapReader},
    structs::Location,
};
use smallvec::SmallVec;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio;
use tokio::runtime::Handle;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::Semaphore;
use tokio::task;
use tokio::task::JoinSet;
use tokio_stream::StreamExt;
use tonic::transport::Channel;
use url::Url;

/// Represents a website to crawl and gather all links.
/// ```rust
/// use website_crawler::spider::website::Website;
/// use website_crawler::rpc::client::create_client;
/// async fn crawl() {
///     let mut client = create_client().await.unwrap();
///     let mut localhost = Website::new("http://example.com");
///     localhost.crawl_grpc(&mut client, Default::default()).await;
///     // `Website` will be filled with `Pages` when crawled. To get them, just use
///     for page in localhost.get_pages() {
///         // do something
///     }
/// }
/// ```
#[derive(Debug)]
pub struct Website {
    /// configuration properties for website.
    pub configuration: Box<Configuration>,
    /// the domain name of the website
    domain: Box<CompactString>,
    /// contains all non-visited URL.
    links: HashSet<CaseInsensitiveString>,
    /// contains all visited URL.
    links_visited: Box<HashSet<CaseInsensitiveString>>,
    /// Robot.txt parser holder.
    robot_file_parser: Option<Box<RobotFileParser>>,
    /// current sitemap url
    sitemap_url: Option<Box<CompactString>>,
    /// the domain url parsed
    domain_parsed: Option<Box<Url>>,
}

/// hashset string_concat
pub type Message = HashSet<CaseInsensitiveString>;

/// shared data
pub type Shared = Pin<
    Arc<(
        Client,
        (CompactString, SmallVec<[CompactString; 2]>),
        AtomicBool,
    )>,
>;

lazy_static! {
    static ref SEM: Semaphore = {
        let logical = num_cpus::get();
        let physical = num_cpus::get_physical();

        let sem_limit = if logical > physical {
            (logical) / (physical) as usize
        } else {
            logical
        };

        let (sem_limit, sem_max) = if logical == physical {
            (sem_limit * physical, 50)
        } else {
            (sem_limit * 4, 25)
        };

        Semaphore::const_new(sem_limit.max(sem_max))
    };
}

impl Website {
    /// Initialize Website object with a start link to crawl.
    pub fn new(domain: &str) -> Self {
        Self {
            configuration: Box::new(Configuration::new()),
            links_visited: HashSet::new().into(),
            domain: Box::new(CompactString::new(domain)),
            links: HashSet::from([domain.into()]), // todo: remove dup mem usage for domain tracking
            robot_file_parser: None,
            sitemap_url: Default::default(),
            domain_parsed: match url::Url::parse(domain) {
                Ok(u) => Some(Box::new(crate::spider::page::convert_abs_path(&u, "/"))),
                _ => None,
            },
        }
    }

    /// page clone
    pub fn get_pages(&self) -> Vec<Page> {
        self.links_visited
            .iter()
            .map(|l| build(&l.inner(), Default::default()))
            .collect()
    }

    /// links visited getter
    pub fn get_links(&self) -> &HashSet<CaseInsensitiveString> {
        &self.links_visited
    }

    /// crawl delay getter
    fn get_delay(&self) -> Duration {
        Duration::from_millis(*self.configuration.delay)
    }

    /// configure the robots parser on initial crawl attempt and run
    pub async fn configure_robots_parser(&mut self, client: &Client) {
        if self.configuration.respect_robots_txt {
            let robot_file_parser = self
                .robot_file_parser
                .get_or_insert_with(|| RobotFileParser::new());

            if robot_file_parser.mtime() <= 4000 {
                let host_str = match &self.domain_parsed {
                    Some(domain) => &*domain.as_str(),
                    _ => &self.domain,
                };
                if host_str.ends_with("/") {
                    robot_file_parser.read(&client, &host_str).await;
                } else {
                    robot_file_parser
                        .read(&client, &string_concat!(host_str, "/"))
                        .await;
                }
                self.configuration.delay = Box::new(
                    robot_file_parser
                        .get_crawl_delay(&self.configuration.user_agent) // returns the crawl delay in seconds
                        .unwrap_or_else(|| self.get_delay())
                        .as_millis() as u64,
                );
            }
        }
    }

    /// configure http client
    fn configure_http_client(&mut self) -> Client {
        lazy_static! {
            static ref HEADERS: HeaderMap<HeaderValue> = {
                let mut headers = HeaderMap::new();
                headers.insert(CONNECTION, HeaderValue::from_static("keep-alive"));

                headers
            };
        }

        let host_str = match url::Url::parse(&self.domain) {
            Ok(u) => Some(u),
            _ => None,
        };
        let default_policy = reqwest::redirect::Policy::default();
        let policy = match host_str {
            Some(host_s) => reqwest::redirect::Policy::custom(move |attempt| {
                if attempt.url().host_str() != host_s.host_str() {
                    attempt.stop()
                } else {
                    default_policy.redirect(attempt)
                }
            }),
            _ => default_policy,
        };

        let mut client = Client::builder()
            .default_headers(HEADERS.clone())
            .redirect(policy)
            .tcp_keepalive(Duration::from_millis(500))
            .pool_idle_timeout(None)
            .user_agent(match &self.configuration.user_agent {
                Some(ua) => ua.as_str(),
                _ => &get_ua(),
            })
            .brotli(true);

        match &self.configuration.proxy {
            Some(proxy_url) => {
                match reqwest::Proxy::all(proxy_url.as_str()) {
                    Ok(proxy) => match Url::parse(&*proxy_url.as_str()) {
                        Ok(url) => {
                            let password = match &url.password() {
                                Some(pass) => pass,
                                _ => "",
                            };
                            client = client.proxy(proxy.basic_auth(&url.username(), &password));
                        }
                        _ => {
                            client = client.proxy(proxy);
                        }
                    },
                    _ => {
                        log("proxy connect error", "");
                    }
                };
            }
            _ => {}
        }

        match &self.configuration.request_timeout {
            Some(t) => client.timeout(**t),
            _ => client,
        }
        .build()
        .unwrap_or_default()
    }

    /// setup config for crawl
    pub async fn setup(&mut self) -> Client {
        let client = self.configure_http_client();
        self.configure_robots_parser(&client).await;

        client
    }

    /// Start to crawl website with gRPC streams
    pub async fn crawl_grpc(
        &mut self,
        rpc_client: &mut WebsiteServiceClient<Channel>,
        user_id: u32,
    ) {
        let client = self.setup().await;

        self.crawl_concurrent_rpc(client, rpc_client, user_id).await;
    }

    /// Start to crawl website with async conccurency
    pub async fn crawl(&mut self) {
        let client = self.setup().await;

        self.crawl_concurrent(client).await;
    }

    /// Start to crawl website concurrently
    async fn crawl_concurrent(&mut self, client: Client) {
        // valid url to crawl
        if self.domain_parsed.is_some() {
            let throttle = self.get_delay();
            let mut new_links: HashSet<CaseInsensitiveString> = HashSet::new();

            let selectors = get_page_selectors(
                &self.domain_parsed.as_ref().unwrap(),
                self.configuration.subdomains,
                self.configuration.tld,
            );

            if selectors.is_some() {
                let shared: Pin<Arc<(Client, (CompactString, SmallVec<[CompactString; 2]>))>> =
                    Arc::pin((client, unsafe { selectors.unwrap_unchecked() }));
                // crawl while links exists
                while !self.links.is_empty() {
                    let (tx, mut rx): (UnboundedSender<Message>, UnboundedReceiver<Message>) =
                        unbounded_channel();

                    let stream = tokio_stream::iter(&self.links).throttle(throttle);
                    tokio::pin!(stream);

                    while let Some(link) = stream.next().await {
                        if !self.is_allowed(&link) {
                            continue;
                        }
                        log("fetch", &link);
                        self.links_visited.insert(link.clone());
                        let permit = SEM.acquire().await.unwrap();

                        let tx = tx.clone();
                        let shared = shared.clone();
                        let link = link.clone();

                        task::spawn(async move {
                            {
                                let page = Page::new(&link.inner(), &shared.0).await;
                                let links = page.links(&shared.1).await;
                                drop(permit);

                                if let Err(_) = tx.send(links) {
                                    log("receiver dropped", "");
                                }
                            }
                        });
                    }

                    drop(tx);

                    while let Some(msg) = rx.recv().await {
                        new_links.extend(msg);
                    }

                    self.links.clone_from(&(&new_links - &self.links_visited));

                    new_links.clear();

                    if new_links.capacity() > 100 {
                        new_links.shrink_to_fit()
                    }
                }
            }
        }
    }

    /// Start to crawl website concurrently using gRPC callback
    async fn crawl_concurrent_rpc(
        &mut self,
        client: Client,
        rpcx: &mut WebsiteServiceClient<Channel>,
        user_id: u32,
    ) {
        if self.domain_parsed.is_some() {
            let selectors = get_page_selectors(
                &self.domain_parsed.as_ref().unwrap(),
                self.configuration.subdomains,
                self.configuration.tld,
            );

            if selectors.is_some() {
                let shared: Shared = Arc::pin((
                    client,
                    unsafe { selectors.unwrap_unchecked() },
                    AtomicBool::new(true),
                ));
                let rpcx = Arc::new(rpcx);
                let throttle = Box::pin(self.get_delay());
                let chandle = Handle::current();

                if self.configuration.sitemap {
                    self.sitemap_crawl(&shared, &rpcx, &throttle, user_id, &chandle)
                        .await;
                } else {
                    self.inner_crawl(&shared, &rpcx, &throttle, user_id, &chandle)
                        .await;
                }
            }
        }
    }

    /// inner crawl pages until no links exist
    async fn inner_crawl(
        &mut self,
        shared: &Shared,
        rpcx: &Arc<&mut WebsiteServiceClient<Channel>>,
        throttle: &Duration,
        user_id: u32,
        chandle: &Handle,
    ) {
        let mut set: JoinSet<HashSet<CaseInsensitiveString>> = JoinSet::new();
        let mut links: HashSet<CaseInsensitiveString> = self.links.drain().collect();

        loop {
            let stream =
                tokio_stream::iter::<HashSet<CaseInsensitiveString>>(links.drain().collect())
                    .throttle(*throttle);
            tokio::pin!(stream);

            while let Some(link) = stream.next().await {
                if !shared.2.load(Ordering::Relaxed) {
                    set.shutdown().await;
                    break;
                }
                if !self.is_allowed(&link) {
                    continue;
                }
                self.links_visited.insert(link.clone());
                log("fetch", &link);

                self.rpc_callback(&rpcx, &shared, &mut set, &link.inner(), user_id, chandle)
                    .await;
            }

            if links.capacity() >= 1500 {
                links.shrink_to_fit();
            }

            while let Some(res) = set.join_next().await {
                if !shared.2.load(Ordering::Relaxed) {
                    break;
                }
                match res {
                    Ok(msg) => {
                        links.extend(&msg - &self.links_visited);
                    }
                    _ => (),
                };
            }

            if links.is_empty() || !shared.2.load(Ordering::Relaxed) {
                break;
            }
        }
    }

    /// get the entire list of urls in a sitemap
    pub async fn sitemap_crawl(
        &mut self,
        shared: &Shared,
        rpcx: &Arc<&mut WebsiteServiceClient<Channel>>,
        throttle: &Duration,
        user_id: u32,
        chandle: &Handle,
    ) {
        let domain = self.domain.as_str();

        let (sitemap_path, needs_trailing) = match &self.configuration.sitemap_path {
            Some(sitemap_path) => {
                let sitemap_path = sitemap_path.as_str();

                if domain.ends_with('/') && sitemap_path.starts_with('/') {
                    (&sitemap_path[1..], false)
                } else if !domain.ends_with('/')
                    && !sitemap_path.is_empty()
                    && !sitemap_path.starts_with('/')
                {
                    (sitemap_path, true)
                } else {
                    (sitemap_path, false)
                }
            }
            _ => ("sitemap.xml", !domain.ends_with("/")),
        };

        self.sitemap_url = Some(Box::new(
            string_concat!(domain, if needs_trailing { "/" } else { "" }, sitemap_path).into(),
        ));

        // init the base crawl and extend sitemap results after
        self.inner_crawl(&shared, &rpcx, &throttle, user_id, &chandle)
            .await;

        while shared.2.load(Ordering::Relaxed) && !self.sitemap_url.is_none() {
            let mut sitemap_added = false;

            match shared
                .0
                .get(self.sitemap_url.as_ref().unwrap().as_str())
                .send()
                .await
            {
                Ok(response) => {
                    match response.text().await {
                        Ok(text) => {
                            let mut stream =
                                tokio_stream::iter(SiteMapReader::new(text.as_bytes()));

                            while let Some(entity) = stream.next().await {
                                if !shared.2.load(Ordering::Relaxed) {
                                    break;
                                };
                                match entity {
                                    SiteMapEntity::Url(url_entry) => match url_entry.loc {
                                        Location::Url(url) => {
                                            let link = url.as_str().into();

                                            if !self.is_allowed(&link) {
                                                continue;
                                            }

                                            self.links.insert(link);

                                            // perform re-walk inside per link gathered
                                            self.inner_crawl(
                                                &shared, &rpcx, &throttle, user_id, &chandle,
                                            )
                                            .await;
                                        }
                                        Location::None | Location::ParseErr(_) => (),
                                    },
                                    SiteMapEntity::SiteMap(sitemap_entry) => {
                                        match sitemap_entry.loc {
                                            Location::Url(url) => {
                                                self.sitemap_url
                                                    .replace(Box::new(url.as_str().into()));

                                                sitemap_added = true;
                                            }
                                            Location::None | Location::ParseErr(_) => (),
                                        }
                                    }
                                    SiteMapEntity::Err(err) => {
                                        log("incorrect sitemap error: ", err.msg())
                                    }
                                };
                            }
                        }
                        Err(err) => log("http parse error: ", err.to_string()),
                    };
                }
                Err(err) => log("http network error: ", err.to_string()),
            };

            if !sitemap_added {
                self.sitemap_url = None;
            };
        }
    }

    /// perform the rpc callback on new link
    async fn rpc_callback(
        &self,
        rpcx: &WebsiteServiceClient<Channel>,
        shared: &Shared,
        set: &mut JoinSet<HashSet<CaseInsensitiveString>>,
        link: &CompactString,
        user_id: u32,
        chandle: &Handle,
    ) {
        let permit = SEM.acquire().await.unwrap();
        let mut rpcx = rpcx.clone();
        let shared = shared.clone();
        let link = link.to_string();

        task::yield_now().await;

        set.spawn_on(
            async move {
                let page = Page::new(&link, &shared.0).await;
                let links = page.links(&shared.1).await;

                let x = monitor(&mut rpcx, link, user_id, page.html.unwrap_or_default()).await;
                drop(permit);

                if !x {
                    shared.2.store(false, Ordering::Relaxed);
                }

                links
            },
            &chandle,
        );

        task::yield_now().await;
    }

    /// return `true` if URL:
    ///
    /// - is not already crawled
    /// - is not blacklisted
    /// - is not forbidden in robot.txt file (if parameter is defined)
    #[inline]
    pub fn is_allowed(&self, link: &CaseInsensitiveString) -> bool {
        if self.links_visited.contains(link) {
            false
        } else {
            self.is_allowed_default(&link.inner())
        }
    }

    /// return `true` if URL:
    ///
    /// - is not blacklisted
    /// - is not forbidden in robot.txt file (if parameter is defined)
    #[inline]
    pub fn is_allowed_default(&self, link: &CompactString) -> bool {
        if !self.configuration.blacklist_url.is_none() {
            match &self.configuration.blacklist_url {
                Some(v) => !contains(v, &link),
                _ => true,
            }
        } else {
            self.is_allowed_robots(&link)
        }
    }

    /// return `true` if URL:
    ///
    /// - is not forbidden in robot.txt file (if parameter is defined)
    pub fn is_allowed_robots(&self, link: &str) -> bool {
        if self.configuration.respect_robots_txt {
            let robot_file_parser = self.robot_file_parser.as_ref().unwrap(); // unwrap will always return

            robot_file_parser.can_fetch("*", &link)
        } else {
            true
        }
    }
}

#[tokio::test]
async fn test_respect_robots_txt() {
    let mut website: Website = Website::new("https://stackoverflow.com");
    website.configuration.respect_robots_txt = true;
    website.configuration.user_agent = Some(Box::new("*".into()));

    let client = website.setup().await;

    website.configure_robots_parser(&client).await;

    assert_eq!(*website.configuration.delay, 0);

    assert!(!&website.is_allowed(&"https://stackoverflow.com/posts/".into(),));

    // test match for bing bot
    let mut website_second: Website = Website::new("https://www.mongodb.com");
    website_second.configuration.respect_robots_txt = true;
    website_second.configuration.user_agent = Some(Box::new("bingbot".into()));

    let client_second = website_second.setup().await;
    website_second.configure_robots_parser(&client_second).await;

    assert_eq!(*website_second.configuration.delay, 60000); // should equal one minute in ms

    // test crawl delay with wildcard agent [DOES not work when using set agent]
    let mut website_third: Website = Website::new("https://www.mongodb.com");
    website_third.configuration.respect_robots_txt = true;
    let client_third = website_third.setup().await;

    website_third.configure_robots_parser(&client_third).await;

    assert_eq!(*website_third.configuration.delay, 10000); // should equal 10 seconds in ms
}

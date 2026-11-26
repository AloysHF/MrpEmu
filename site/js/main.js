/* MrpEmu site behaviour: bilingual labels, compatibility gallery, counters,
   reveal animation and the mobile menu. Gallery data ships in compat-data.js. */

(function () {
  "use strict";

  var LANG_KEY = "mrpemu-site-lang";
  var I18N = {
    zh: {
      "nav.features": "能力", "nav.gallery": "游戏库", "nav.architecture": "架构",
      "nav.quickstart": "快速开始", "nav.play": "在线试玩",
      "hero.eyebrow": "MYTHROAD / MRP / RUST",
      "hero.title": "让功能机游戏<br><span class=\"accent\">重新跑起来</span>",
      "hero.sub": "MrpEmu 用 Rust 复刻 Mythroad 运行时。固定时钟回放、可脚本化按键，桌面、libretro 与浏览器共用同一颗核心。",
      "hero.play": "体验浏览器核心（实验性）→", "hero.compat": "查看兼容性表", "hero.release": "下载发行版",
      "hero.note": "游戏文件需自行准备并合法获取；本项目不分发任何 .mrp 游戏数据。",
      "phone.left": "菜单", "phone.right": "返回",
      "stat.titles": "款游戏已截图", "stat.resolution": "典型分辨率", "stat.frontends": "套前端", "stat.targets": "个构建目标",
      "about.title": "一个核心，三种玩法", "about.sub": "从 .mrp 包到画面输出，路径保持简单可审计。",
      "flow.package": "资源包 + 设备文件", "flow.output": "画面与按键", "flow.outputSub": "桌面 / 手柄 / 浏览器",
      "features.title": "为可复现而设计", "features.sub": "不是“能打开就算兼容”，而是每次都能跑出同一帧。",
      "f1.title": "确定性回放", "f1.body": "固定 30 Hz guest 时钟，不用墙钟计时；同一个包和同一串按键在任意机器上得到相同帧序列。",
      "f2.title": "脚本化按键", "f2.body": "按帧注入软键、方向键和数字键，把启动提示一路走到标题、菜单甚至游戏画面。",
      "f3.title": "截图即证据", "f3.body": "无头截图记录非零像素与颜色数；单色帧直接拒绝，不拿黑屏冒充兼容。",
      "f4.title": "105 款实测", "f4.body": "热门游戏语料逐款捕获，状态与采集配方逐条登记；不验证游戏内操作就不标 Playable。",
      "f5.title": "三端共用核心", "f5.body": "桌面前端、libretro 核心与 EmulatorJS 浏览器核心都调用同一套 mrpemu-core 行为。",
      "f6.title": "本地运行", "f6.body": "浏览器模式用 blob URL 读取你选中的文件，不上传、不托管任何游戏数据。",
      "gallery.title": "游戏画面库", "gallery.sub": "105 张真实捕获帧。点开状态标签可以筛选；每张卡片都写明分辨率与采集方式。",
      "filter.all": "全部", "filter.partial": "交互画面", "filter.boots": "启动画面", "filter.network": "网络受限",
      "gallery.search": "搜索游戏", "gallery.empty": "没有匹配的游戏。", "gallery.more": "显示更多",
      "gallery.foot": "画面说明与完整采集配方见兼容性文档。截图只证明画面被渲染，不代表完整流程或音频已验证。",
      "arch.title": "架构一览", "arch.frontend": "前端", "arch.target": "目标平台",
      "quick.title": "三步开始", "quick.build": "构建", "quick.buildBody": "桌面前端不依赖游戏数据即可编译。",
      "quick.run": "运行", "quick.runBody": "传入合法获取的 .mrp 文件；可选 --system-dir 加载设备文件。",
      "quick.capture": "复现截图", "quick.captureBody": "或用批处理脚本一次捕获整个语料库。",
      "play.eyebrow": "BROWSER MODE", "play.title": "先试一下，再决定要不要装",
      "play.sub": "在浏览器里选择本地 .mrp 文件，由 EmulatorJS + WASM 核心直接运行。核心仍属实验性，并非所有游戏都能运行；文件不会离开你的设备。",
      "play.button": "打开在线试玩 →",
      "footer.about": "用 Rust 写的 Mythroad / MRP 应用模拟器。", "footer.project": "项目", "footer.releases": "发行版",
      "footer.docs": "文档", "footer.compat": "兼容性表", "footer.standalone": "桌面前端", "footer.arch": "架构",
      "footer.disclaimer": "游戏数据版权归各自权利人所有。"
    },
    en: {
      "nav.features": "Features", "nav.gallery": "Gallery", "nav.architecture": "Architecture",
      "nav.quickstart": "Quick start", "nav.play": "Play in browser",
      "hero.eyebrow": "MYTHROAD / MRP / RUST",
      "hero.title": "Feature-phone games<br><span class=\"accent\">back in action</span>",
      "hero.sub": "MrpEmu reimplements the Mythroad runtime in Rust. Fixed-clock replay, scripted keys and one core shared by desktop, libretro and the browser.",
      "hero.play": "Try the browser core (experimental) →", "hero.compat": "Compatibility table", "hero.release": "Get a release",
      "hero.note": "Bring your own legally obtained game files; this project ships no .mrp data.",
      "phone.left": "MENU", "phone.right": "BACK",
      "stat.titles": "games captured", "stat.resolution": "typical display", "stat.frontends": "frontends", "stat.targets": "build targets",
      "about.title": "One core, three ways in", "about.sub": "From an .mrp package to pixels, the path stays short and auditable.",
      "flow.package": "package + device files", "flow.output": "Pixels and keys", "flow.outputSub": "desktop / gamepad / browser",
      "features.title": "Built for reproducibility", "features.sub": "Not just “it opens”, but the same frame every time.",
      "f1.title": "Deterministic replay", "f1.body": "A fixed 30 Hz guest clock, never wall time: the same package and key schedule produce the same frames on any machine.",
      "f2.title": "Scripted keys", "f2.body": "Inject soft keys, directions and digits at exact frames to walk from an audio prompt into a title, menu or game.",
      "f3.title": "Screenshots as evidence", "f3.body": "Headless captures report non-zero pixels and color count; single-color frames are rejected instead of passed off as compatible.",
      "f4.title": "105 titles measured", "f4.body": "Every title in the hot-game corpus is captured, with its status and recipe recorded. No Playable rating without verified in-game controls.",
      "f5.title": "Three frontends, one core", "f5.body": "Desktop, libretro and the EmulatorJS browser core all call the same mrpemu-core behaviour.",
      "f6.title": "Local by design", "f6.body": "Browser mode reads your selected file through a blob URL. Nothing is uploaded or hosted.",
      "gallery.title": "Game gallery", "gallery.sub": "105 real captured frames. Filter by status; each card records display size and capture recipe.",
      "filter.all": "All", "filter.partial": "Interactive", "filter.boots": "Startup", "filter.network": "Network blocked",
      "gallery.search": "Search games", "gallery.empty": "No matching games.", "gallery.more": "Show more",
      "gallery.foot": "Observed notes and exact recipes live in the compatibility document. A screenshot proves a screen rendered, not a full playthrough or working audio.",
      "arch.title": "Architecture at a glance", "arch.frontend": "Frontends", "arch.target": "Targets",
      "quick.title": "Three steps in", "quick.build": "Build", "quick.buildBody": "The desktop frontend builds without any game data.",
      "quick.run": "Run", "quick.runBody": "Pass a legally obtained .mrp file; optionally add --system-dir for device files.",
      "quick.capture": "Reproduce a capture", "quick.captureBody": "Or capture the whole corpus with the batch script.",
      "play.eyebrow": "BROWSER MODE", "play.title": "Try before you install",
      "play.sub": "Pick a local .mrp file in the browser and run it through the EmulatorJS + WASM core. The core is experimental and not every game runs; the file never leaves your device.",
      "play.button": "Open browser player →",
      "footer.about": "A Mythroad / MRP emulator written in Rust.", "footer.project": "Project", "footer.releases": "Releases",
      "footer.docs": "Docs", "footer.compat": "Compatibility", "footer.standalone": "Desktop frontend", "footer.arch": "Architecture",
      "footer.disclaimer": "Game data remains the property of its respective owners."
    }
  };

  var currentLang = "zh";
  try {
    currentLang = localStorage.getItem(LANG_KEY) === "en" ? "en" : "zh";
  } catch (e) { /* storage may be unavailable */ }

  function applyLang(lang) {
    currentLang = lang;
    document.documentElement.lang = lang === "zh" ? "zh-CN" : "en";
    var dict = I18N[lang];
    var nodes = document.querySelectorAll("[data-i18n]");
    for (var i = 0; i < nodes.length; i++) {
      var key = nodes[i].getAttribute("data-i18n");
      if (dict[key] !== undefined) nodes[i].innerHTML = dict[key];
    }
    var toggle = document.getElementById("lang-toggle");
    if (toggle) toggle.textContent = lang === "zh" ? "EN" : "中文";
    document.title = lang === "zh"
      ? "MrpEmu — MRP 游戏，回到桌面"
      : "MrpEmu — MRP games, back in action";
    try { localStorage.setItem(LANG_KEY, lang); } catch (e) { /* ignore */ }
  }

  function setupLang() {
    var toggle = document.getElementById("lang-toggle");
    if (!toggle) return;
    toggle.addEventListener("click", function () {
      applyLang(currentLang === "zh" ? "en" : "zh");
      renderGallery();
    });
  }

  function setupBurger() {
    var burger = document.getElementById("nav-burger");
    var links = document.getElementById("nav-links");
    if (!burger || !links) return;
    burger.addEventListener("click", function () {
      var open = links.classList.toggle("open");
      burger.setAttribute("aria-expanded", open ? "true" : "false");
    });
    links.addEventListener("click", function (event) {
      if (event.target.tagName === "A") {
        links.classList.remove("open");
        burger.setAttribute("aria-expanded", "false");
      }
    });
  }

  function setupReveal() {
    if (!("IntersectionObserver" in window)) return;
    var targets = document.querySelectorAll(".section-title, .section-sub, .feature-card, .quick-card, .arch-box, .game-card, .stat, .flow-node");
    for (var i = 0; i < targets.length; i++) targets[i].classList.add("reveal");
    var observer = new IntersectionObserver(function (entries) {
      entries.forEach(function (entry) {
        if (entry.isIntersecting) {
          entry.target.classList.add("visible");
          observer.unobserve(entry.target);
        }
      });
    }, { threshold: 0.1 });
    targets.forEach(function (node) { observer.observe(node); });
  }

  function setupCounters() {
    var nodes = document.querySelectorAll("[data-count]");
    function run(node) {
      var target = Number(node.getAttribute("data-count"));
      var start = performance.now();
      function frame(now) {
        var p = Math.min(1, (now - start) / 1100);
        node.textContent = String(Math.round(target * (1 - Math.pow(1 - p, 3))));
        if (p < 1) requestAnimationFrame(frame);
      }
      requestAnimationFrame(frame);
    }
    if (!("IntersectionObserver" in window)) {
      nodes.forEach(run);
      return;
    }
    var observer = new IntersectionObserver(function (entries) {
      entries.forEach(function (entry) {
        if (entry.isIntersecting) {
          run(entry.target);
          observer.unobserve(entry.target);
        }
      });
    }, { threshold: 0.5 });
    nodes.forEach(function (node) { observer.observe(node); });
  }

  function docsRoot() {
    // Deployed site lives at the repository root; a local checkout serves
    // index.html from site/, so docs/ is one level up there.
    var path = window.location.pathname;
    if (/(^|\/)site\/(index\.html)?$/.test(path)) return new URL("../docs/", window.location.href);
    return new URL("docs/", window.location.href);
  }

  function imageUrl(entry) {
    return new URL(entry.image, docsRoot()).href;
  }

  function statusClass(status) {
    if (status === "Partial") return "badge-Partial";
    if (status.indexOf("Blocked") === 0) return "badge-Network";
    return "badge-Boots";
  }

  var activeFilter = "all";
  var query = "";
  var visibleCount = 24;
  var PAGE = 24;

  function renderGallery() {
    var grid = document.getElementById("gallery-grid");
    if (!grid) return;
    var data = window.MRP_COMPAT || [];
    var needle = query.trim().toLowerCase();
    var filtered = data.filter(function (entry) {
      var matchesFilter = activeFilter === "all" || entry.status === activeFilter;
      var haystack = (entry.title + " " + entry.package + " " + entry.note).toLowerCase();
      return matchesFilter && (!needle || haystack.indexOf(needle) !== -1);
    });

    grid.textContent = "";
    var slice = filtered.slice(0, visibleCount);
    slice.forEach(function (entry) {
      var card = document.createElement("article");
      card.className = "game-card";

      var shot = document.createElement("div");
      shot.className = "game-shot";
      var img = document.createElement("img");
      img.src = imageUrl(entry);
      img.alt = entry.title + " capture";
      img.loading = "lazy";
      img.width = 240;
      img.height = 320;
      var num = document.createElement("span");
      num.className = "game-num";
      num.textContent = "#" + entry.n;
      shot.appendChild(img);
      shot.appendChild(num);

      var info = document.createElement("div");
      info.className = "game-info";
      var title = document.createElement("h3");
      title.className = "game-title";
      title.textContent = entry.title;
      title.title = entry.package;
      var meta = document.createElement("div");
      meta.className = "game-meta";
      var badge = document.createElement("span");
      badge.className = "badge " + statusClass(entry.status);
      badge.textContent = entry.status;
      var screen = document.createElement("span");
      screen.textContent = entry.screen;
      var capture = document.createElement("span");
      capture.textContent = entry.capture;
      meta.appendChild(badge);
      meta.appendChild(screen);
      meta.appendChild(capture);
      var note = document.createElement("p");
      note.className = "game-note";
      note.textContent = entry.note;
      info.appendChild(title);
      info.appendChild(meta);
      info.appendChild(note);

      card.appendChild(shot);
      card.appendChild(info);
      grid.appendChild(card);
    });

    document.getElementById("gallery-empty").hidden = filtered.length !== 0;
    var more = document.getElementById("gallery-more");
    more.hidden = filtered.length <= visibleCount;
    more.textContent = I18N[currentLang]["gallery.more"] +
      " (" + Math.min(visibleCount, filtered.length) + "/" + filtered.length + ")";
  }

  function setupGallery() {
    var filters = document.getElementById("gallery-filters");
    if (filters) {
      filters.addEventListener("click", function (event) {
        var button = event.target.closest("button[data-filter]");
        if (!button) return;
        visibleCount = PAGE;
        activeFilter = button.getAttribute("data-filter");
        filters.querySelectorAll(".chip").forEach(function (chip) {
          chip.classList.toggle("is-active", chip === button);
        });
        renderGallery();
      });
    }
    var search = document.getElementById("gallery-search");
    if (search) {
      search.addEventListener("input", function () {
        query = search.value;
        visibleCount = PAGE;
        renderGallery();
      });
    }
    var more = document.getElementById("gallery-more");
    if (more) {
      more.addEventListener("click", function () {
        visibleCount += PAGE;
        renderGallery();
      });
    }

    var data = window.MRP_COMPAT || [];
    var heroShot = document.getElementById("hero-shot");
    if (heroShot && data.length) {
      var hero = data.filter(function (entry) { return entry.title === "大富翁"; })[0] || data[0];
      heroShot.src = imageUrl(hero);
      heroShot.alt = hero.title;
    }
    renderGallery();
  }

  applyLang(currentLang);
  setupLang();
  setupBurger();
  setupReveal();
  setupCounters();
  setupGallery();
})();

# Platform Guide Coverage

The website’s connection guides now cover provider preparation, exact local fields, verification and failure recovery. Instructions follow the current read-only RepoTunnel source and official provider documentation.

| Guide | Practical setup | Official destinations | Visual reference |
|---|---|---|---|
| ngrok | Account, authtoken value, Connect fields, Ready, health and OAuth verification | Signup, dashboard, authtoken page, quickstart, pricing, status | Public signed-out signup page |
| Cloudflare | Install connector, named tunnel token, published route, localhost origin, app connection, health | Dashboard, downloads, setup, troubleshooting, Quick Tunnel limits | Official tutorial’s published-application dialog |
| Direct HTTPS | Linux prerequisites, Route64 WireGuard, routing, DuckDNS, optional Netiter IPv4, trusted certificate, health and OAuth | Route64 manager, DuckDNS/specification, Netiter, WireGuard, Certbot, Let’s Encrypt | Public Route64 login and DuckDNS pages |
| Local model chat | Install one supported runtime, load a model, expose its local server, select it in Home | Ollama, LM Studio, llama.cpp | Text walkthrough; no invented runtime screenshots |
| ChatGPT MCP app | Prepare endpoint, current client controls, OAuth and actual workspace read | Current OpenAI developer mode/MCP app documentation | Numbered client workflow |
| Continuation companion | Source folder, Developer mode, Load unpacked, exact conversation selection | RepoTunnel extension source/privacy, Chrome and Edge official guides | Official Google unpacked-extension example |
| Phone | Local wireless pairing, device selection and human capability/Accessibility grants | Android developer options and Wi-Fi pairing documentation | Official Google Wireless debugging example |
| Git and video | Authentication/install references and media QA references | GitHub, Git, yt-dlp, FFmpeg, ffprobe | Contextual documentation links |

All 40 documentation guides, 16 product pages and 9 solution pages were reviewed for useful setup and follow-on links. Existing installation/download/security pages already expose their official destinations. Provider choices are alternatives rather than a requirement to enable every service.

Verification is recorded in the source/static checks and `guide-browser-report.json`. `platform-link-report.json` records direct HTTP checks; protected provider dashboards can require sign-in or reject automated requests, so their official documentation and exact source-linked destinations are also reviewed. `platform-screenshots.json` contains only the six images actually used, with source, dimensions and hashes.

The main RepoTunnel project was inspected read-only. The website working tree remains uncommitted for review, with its existing saved second-version HEAD preserved.

%global debug_package %{nil}

Name:           cas
Version:        0.1.28
Release:        1%{?dist}
Summary:        ChatGPT account switcher for Codex
License:        Apache-2.0
Source0:        cas

%description
CAS stores multiple Codex ChatGPT credentials under the current user's
.codex/cas directory and switches the active .codex/auth.json credential.

%prep

%build

%install
install -Dpm0755 %{SOURCE0} %{buildroot}%{_bindir}/cas

%files
%{_bindir}/cas

%changelog
* Fri Oct 02 2026 CAS Project <cas@localhost> - 0.1.28-1
- Add concurrent test/refresh command for all saved ChatGPT credentials
- Send a clean streaming hello to gpt-6-luna at the lowest advertised reasoning effort
- Print model responses, or redacted request headers when a request fails

* Fri Oct 02 2026 CAS Project <cas@localhost> - 0.1.27-1
- Color status and account-selector fields by semantic segment
- Render remaining usage on a smooth red-yellow-green 0-100 percent gradient
- Keep redirected output and NO_COLOR output free of ANSI escapes

* Fri Oct 02 2026 CAS Project <cas@localhost> - 0.1.26-1
- Show the local reset date and time next to 5-hour and long-window usage limits

* Fri Oct 02 2026 CAS Project <cas@localhost> - 0.1.25-1
- Rename selector cancel row to No changes / 不做更改 to avoid implying CAS exits

* Fri Oct 02 2026 CAS Project <cas@localhost> - 0.1.24-1
- Remove all Windows-specific source and build configuration
- Drop legacy platform-specific process-name compatibility and use direct Unix filesystem semantics

* Fri Oct 02 2026 CAS Project <cas@localhost> - 0.1.23-1
- Scan the live .codex/auth.json during status and report the effective account
- Use a stable read for current-account detection instead of trusting registry state

* Fri Oct 02 2026 CAS Project <cas@localhost> - 0.1.22-1
- Shorten switch success output in Chinese and English

* Fri Oct 02 2026 CAS Project <cas@localhost> - 0.1.21-1
- Deduplicate locale detection between core and CLI
- Merge the two interactive menu implementations into one renderer
- Remove the unused hex dependency and the CLI's redundant sys-locale dependency

* Fri Oct 02 2026 CAS Project <cas@localhost> - 0.1.20-1
- Remove ineffective startup HEAD prewarm after benchmarking showed no latency improvement
- Keep unconditional process-wide HTTP client initialization and connection pooling

* Fri Oct 02 2026 CAS Project <cas@localhost> - 0.1.19-1
- Prewarm the ChatGPT usage HTTPS connection in the background at process startup
- Reuse the warmed HTTP/2 connection for later account usage queries

* Fri Oct 02 2026 CAS Project <cas@localhost> - 0.1.18-1
- Initialize one process-wide HTTP client immediately at startup
- Reuse its connection pool for usage, refresh, and login requests

* Fri Oct 02 2026 CAS Project <cas@localhost> - 0.1.17-1
- Keep command keys untranslated in the localized interactive main menu

* Fri Oct 02 2026 CAS Project <cas@localhost> - 0.1.16-1
- Detect the operating-system locale and use Simplified Chinese for zh locales
- Keep English for non-Chinese locales and remove OK prefixes from success output

* Fri Oct 02 2026 CAS Project <cas@localhost> - 0.1.15-1
- Open an interactive main menu when cas is run without a subcommand
- Put Status first and Help last; use Esc or Ctrl-C to leave the main menu

* Fri Oct 02 2026 CAS Project <cas@localhost> - 0.1.14-1
- Refresh multiple account usage probes concurrently
- Commit one registry update per batch instead of once per account

* Fri Oct 02 2026 CAS Project <cas@localhost> - 0.1.13-1
- Fix interactive selectors leaving duplicate frames after arrow-key navigation
- Track wrapped terminal rows instead of relying on saved cursor positions

* Fri Oct 02 2026 CAS Project <cas@localhost> - 0.1.12-1
- Refresh usage before printing any account selection list
- Show current 5h and long-window remaining usage on every account row

* Fri Oct 02 2026 CAS Project <cas@localhost> - 0.1.11-1
- Show paired command names in top-level help: switch/enable, import/input, remove/delete
- Add import as an alias of input

* Fri Oct 02 2026 CAS Project <cas@localhost> - 0.1.10-1
- Add enable as a visible alias of switch

* Fri Oct 02 2026 CAS Project <cas@localhost> - 0.1.9-1
- Add delete as a visible alias of remove

* Fri Oct 02 2026 CAS Project <cas@localhost> - 0.1.8-1
- Prefer the official chatgpt_user_id claim over JWT subject ids
- Safely migrate registry entries that stored the legacy JWT sub as userId

* Fri Oct 02 2026 CAS Project <cas@localhost> - 0.1.7-1
- Make login save credentials without switching or stopping Codex
- Identify accounts by user plus workspace instead of workspace alone
- Ignore Linux thread entries and stop the owning Codex Desktop process on switch

* Fri Oct 02 2026 CAS Project <cas@localhost> - 0.1.6-1
- Use a black background and white text on the browser OAuth callback page

* Fri Oct 02 2026 CAS Project <cas@localhost> - 0.1.5-1
- Always print the browser OAuth URL while also opening the default browser

* Fri Oct 02 2026 CAS Project <cas@localhost> - 0.1.4-1
- Open browser OAuth automatically and only print the URL on launch failure

* Fri Oct 02 2026 CAS Project <cas@localhost> - 0.1.3-1
- Make account identity workspace-aware and disambiguate selectors
- Preserve the newest credential when duplicates are discovered

* Fri Oct 02 2026 CAS Project <cas@localhost> - 0.1.2-1
- Add browser OAuth login alongside device-code login

* Fri Oct 02 2026 CAS Project <cas@localhost> - 0.1.1-1
- Add CLI login and interactive account selectors

* Thu Oct 01 2026 CAS Project <cas@localhost> - 0.1.0-1
- Initial RPM package

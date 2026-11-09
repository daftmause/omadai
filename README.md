# omadai

omadai shows what you spend on AI APIs, in dollars, per platform and per key, in one place. It reads
spend from each platform's own API with keys you add, on your own machine. On Omarchy it is a bar
icon with a panel and an app window; a macOS tray app over the same Rust core is planned.

Platforms: OpenAI, Anthropic, fal.ai, xAI, Higgsfield, BytePlus, Flux, Fish Audio and Gemini.

## Install on Omarchy

There is no package yet, so both halves come from this checkout:

1. Build the binary with `bin/omadai build` and put `target/release/omadai` on your `PATH`. The
   plugin calls `omadai` and shows "omadai is not installed" when it cannot find it.
2. Install the plugin with `bin/omadai plugin-dev`, then run `omarchy-restart-shell`.
3. Add a key per platform with `omadai key add <platform>`, or from the app window, which opens a
   terminal prompt. The key is checked once and stored in the system keyring.

`omadai` with no arguments prints its commands. `bin/omadai` is a different tool, the repository's
own development CLI; with no arguments it prints its verbs.

## Keyring

Keys live in the system keyring, which must be encrypted. Omarchy creates its default keyring with no
password, which stores its contents in plain text. omadai refuses to store a key there unless you
pass `--allow-unencrypted-keyring`, and warns while it stays that way.

## Setting up each platform

Run `omadai key add <platform>` with the id in parentheses under each platform below. Pass
`--label <name>` to name a key. Input is hidden and each key is checked once before it is stored.

### OpenAI

Needs an Admin API key (`openai`). Admin keys have no scopes and cannot be made read-only. Create one
in the OpenAI organization settings and paste it at the prompt. omadai reads daily costs per API key
from the organization costs endpoint, which keeps one rolling year of history.

### Anthropic

Needs an Admin API key (`anthropic`), created in the Console under Settings, Admin keys. Admin keys
carry full Admin API access; you can give one an expiration. The Admin API exists only for team
(organization) accounts. An individual account has no spend API, and omadai shows a requirement row
for it. Cost data starts in January 2026, runs 4 to 24 hours behind, and is revised for about 7 days
after a month ends.

### fal.ai

Needs a fal key created with the BILLING (or FULL) preset (`fal`). An ordinary key is refused with
403 "not permitted", which omadai shows as a requirement row. Spend is summed per key from the usage
endpoint.

### xAI

Needs a management key and your team ID (`xai`). Create the key in the xAI Console under Settings,
Management Keys, with Billing: Read only and API Keys: Read only; everything else can stay No access.
The console shows an example command with the new key embedded after you create it; copy only the key.
Find the team ID under Settings, Team, and give it at the prompt or with `--team-id <id>`. Spend per
key is an estimate until the platform posts its monthly settlement, so the figure is labelled
estimated. Grok consumer subscriptions expose no spend API and are not covered.

### Higgsfield

Needs the API key the Higgsfield dashboard issues, a single `id:secret` string (`higgsfield`). It works
for a non-enterprise account. Higgsfield labels the figures an estimate, not a billing record. Usage
through a Higgsfield subscription (web app, Blender add-on) is not API usage and does not appear.

### BytePlus

Needs an IAM user's AccessKey ID and Secret Access Key (`byteplus`); omadai prompts for each in turn.
Create an IAM user with the `BillingCenterReadOnlyAccess` policy, which is enough; the user needs no
console access. omadai reads Seedance (ModelArk) bills per model, since those rows carry no API key
ID. Usage drawn from a resource package bills $0, and the package itself bills on its purchase day.

### Flux

Needs any Black Forest Labs API key (`flux`). Flux has no usage API, so omadai reads the credit
balance (1 credit is $0.01) and derives spend from its changes. Spend starts from the first balance
omadai sees.

### Fish Audio

Needs any Fish Audio API key (`fish`). omadai reads API credit and cumulative top-up from the wallet
endpoint and reports lifetime spend as top-up minus remaining credit, which stays correct across
top-ups. Fish Audio does not document a currency unit, so omadai assumes USD. Keys that read one
wallet share one figure.

### Gemini

Gemini (`gemini`) has no spend API. Dollars come from the Cloud Billing standard usage cost export to
BigQuery, read with your own Google sign-in. Figures are per Google Cloud project, not per key.
omadai ships no OAuth client secret; you create your own, once:

1. Create a Google Cloud project used only for the billing export and the OAuth client, such as
   `<your-project>`, linked to your billing account, and enable the BigQuery API on it. Avoid an
   application project: deleting it deletes the export dataset and the OAuth client. A lien with
   restriction `resourcemanager.projects.delete` blocks deleting it by accident.
2. In Billing, Billing export, BigQuery export, turn on Standard usage cost into a dataset in that
   project, such as `<your-project>.billing_export`. The export covers every project on that billing
   account and backfills the previous invoice months the first time it is enabled. Data arrives
   within hours and can take up to 5 days to catch up. The dataset then holds a table named
   `gcp_billing_export_v1_<billing-account-id>`.
3. In APIs & Services, Credentials, create an OAuth client of type Desktop app and note its client ID
   and client secret.
4. Set the OAuth consent screen to Internal if you use a Workspace organization. On a personal
   account use External and In production: in Testing, Google expires refresh tokens after 7 days.
   An unverified External app shows Google's unverified-app screen at sign-in, because the scope
   omadai requests (`cloud-platform.read-only`, the narrowest read-only scope that covers BigQuery
   queries) is sensitive.
   On a Workspace organization whose admin turned on Google Cloud session control, Google asks
   every app that uses Google Cloud scopes to sign in again on a timer of 1 to 24 hours, omadai
   included. To keep omadai signed in, an admin marks the omadai app Trusted in the Admin console's
   App access control and ticks Exempt trusted apps in Google Cloud session control.
5. Make sure the Google account you will sign in with has BigQuery Job User on the project and
   BigQuery Data Viewer on the export dataset.
6. Run `omadai key add gemini`. It asks for the project (`<your-project>`), the dataset
   (`billing_export` by default), the OAuth client ID and the client secret, then opens your browser to
   sign in with Google. It finds the export table itself and saves the project, dataset, table and
   location in omadai's config. One Gemini key per install.

omadai counts Gemini API (AI Studio) and Vertex AI usage net of credits, by UTC usage date, so
a month can differ from the invoice at its edges. Tax and rounding rows are left out.

## Limits

- Fetches never unlock the keyring. If it is locked, a fetch fails with "keyring locked" rather than
  waiting on a prompt.
- Platforms report spend at different delays and granularity, and some only as a balance or an
  estimate; the chip beside each figure says which. Gemini figures are per project, not per key,
  and only as current as Google's billing export.
- Some platforms keep limited history: OpenAI one rolling year, Anthropic from January 2026. A
  refresh of an older month may log a failure for platforms with no recorded floor.
- omadai only reads spend. Removing a key deletes the local copy; the key stays valid at the vendor
  until you revoke it there. For Gemini, `omadai key remove gemini <label>` offers to revoke omadai's
  Google sign-in too. That signs the Google account out of omadai on every machine, and may sign it
  out of other apps whose OAuth client is in the same Google Cloud project.
- The macOS app under `apps/macos` is not built yet.

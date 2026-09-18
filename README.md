# Jeeves

[![CI](https://github.com/Infrawrench/Jeeves/actions/workflows/ci.yml/badge.svg)](https://github.com/Infrawrench/Jeeves/actions/workflows/ci.yml)

Discord moderation rules in plain English. Jeeves uses Jev to interpret messages and
strike history, Gemini to describe image attachments and generate rules that need
computation, and PostgreSQL to keep track of messages, rules, and strikes.

**[Invite Jeeves to your server](https://discord.com/oauth2/authorize?client_id=1550283057125392566&scope=bot%20applications.commands&permissions=76806&integration_type=0)**

Built with Rust, Twilight, Tokio, SQLx, and an embedded QuickJS runtime.

## Get started

1. Invite Jeeves using the link above.
2. Give it access to the channels you want it to moderate, and place its role above
   the members it should be able to strike, kick, or ban.
3. As an administrator, use `/addaction` to describe a rule:

   ```text
   /addaction question:Strike users who make your mum jokes.
   /addaction question:Ban a user when they have at least 3 strikes.
   ```

The invite requests **View Channels**, **Send Messages**, **Manage Messages**,
**Read Message History**, **Kick Members**, and **Ban Members**. Manage Messages lets
Jeeves delete messages that receive strikes; Send Messages lets it notify the member.

Rules belong to the server where they are created. There are no built-in moderation
rules or automatic three-strike bans: add an escalation rule if you want one.
Jeeves displays **Moderating in N servers** as its Playing activity and updates the
count when it joins or leaves a server.

## Commands

| Command | Who can use it | What it does |
| --- | --- | --- |
| `/invite` | Anyone | Get a private invite link for the running bot. |
| `/addaction question:<rule>` | Administrators | Create a message or strike rule from a description. |
| `/manageactions` | Administrators | Browse and remove the server's message and strike rules. |
| `/strike user:@member reason:<reason>` | Members with Moderate Members or Administrator | Record a strike and evaluate strike rules. |
| `/strikes` | Anyone in the server | Privately view their own strikes. |
| `/managestrikes user:@member` | Administrators | View and remove someone's strikes, including former members. |

Moderation commands work in servers and reply privately. Rule descriptions and strike
reasons can contain up to 1,000 characters. `/ping` is also registered, but its health
check is not implemented yet.

Strike and action lists show up to five entries per page, with **Previous** and
**Next** controls. Long entries may reduce the page size. Database IDs stay hidden.
Administrators can remove a numbered entry or use **Clear listed strikes/actions**
with confirmation. Clearing preserves records added since the list was opened;
reopen the command to include them. Controls belong to the person who opened the list,
and admin permissions are checked again on every click.

## Writing rules

Describe the condition and the outcome. Jeeves figures out how to evaluate it; there
is no type selector or separate channel option.

```text
/addaction question:Strike users who post unsolicited advertisements.
/addaction question:Strike users who send the same message 3 times within 30 seconds.
/addaction question:Strike users for spam in #general and #chat.
/addaction question:Ban a user when they have at least 3 strikes.
```

Jev classifies each rule using named choices with descriptions:

| Type | Evaluation |
| --- | --- |
| `message_binary` | Jev interprets the current message, images, and channel context. |
| `message_code` | Gemini generates JavaScript for counts, time windows, and other computations on messages. |
| `strike_binary` | Jev interprets a member's new strike and earlier strikes. |
| `strike_code` | Gemini generates JavaScript for computations such as strike thresholds. |
| `contains_channels` | Gemini separates the channel restrictions from the rule, then Jev classifies the remaining statement. |
| `none_of_the_above` | The request is rejected without saving a rule. |

Rules apply to every channel in the server unless they name specific channels.
Channel mentions are useful when names are ambiguous. Jeeves resolves restrictions
against the server's message channels and active threads before saving. Unknown
channels, exclusions, categories, and unsupported scopes are rejected. A selected
parent channel does not automatically include its threads.

Message rules can produce **ban**, **kick**, **strike**, or **no action**. A matching
Jev message rule with no stated punishment defaults to a strike. Earlier messages
provide context; a violation in history alone should not punish a later message's
author. Strike rules can ban, kick, or take no action; they cannot create more strikes.

Use `/manageactions` to review the saved statement, type, and channel scope. Generated
code is checked for syntax and function shape before saving, but that check does not
prove that every input will produce the intended decision. Unsupported rules and
failed classification or generation save nothing.

## How strikes work

Every matching strike outcome is recorded, even when another rule also requests a
kick or ban. Both automatic strikes and `/strike` run the configured strike rules.
Jeeves combines their outcomes with the message rules: **ban takes precedence over
kick**, and duplicate removal requests collapse into one. All strikes remain recorded.

Before recording a strike, Jeeves checks current roles. The recipient's highest role
must be below the bot's highest role. The server owner and Jeeves itself are protected;
failed membership or role lookups also prevent the strike.

After a new strike, Jeeves mentions the recipient in the channel and points them to
`/strikes`. Automatic strikes also attempt to delete the offending message. Several
strikes from one message produce one notice and one deletion attempt. `/strike` has no
source message to delete. Failed deletion or notification is logged without undoing
the strike or preventing escalation.

Retries of an interaction or the same message/rule pair do not create duplicate
strikes. Strikes persist independently of message retention. Removing a strike stops
it from counting toward future rules; removing strikes or rules does not undo bans
or kicks. A rule already loaded by an in-progress task may finish after its removal.

## Self-hosting

You need Rust **1.94+**, PostgreSQL, a Discord bot token, a TypeSafe API key, and Gemini
access through either Vertex AI or the Gemini Developer API. Docker Compose can run
the development database.

### Discord

Create your own application in the [Discord Developer Portal](https://discord.com/developers/applications).
Enable **Message Content Intent** on its Bot page and leave the Interactions Endpoint
URL unset: Jeeves receives interactions through the gateway.

Install your application with the `bot` and `applications.commands` scopes and the
permissions listed above. The invite at the top of this README is for Jeeves' existing
application. For your own instance, replace its `client_id` with your application's ID,
or use `/invite` after installation. See Discord's
[bot authorization guide](https://docs.discord.com/developers/topics/oauth2#bot-authorization-flow).

### Environment and Gemini

Copy the example, then fill in your credentials:

```sh
cp .env.example .env
```

The example selects **Vertex AI**. Enable the Vertex AI API in your Google Cloud
project, link billing, and give the authenticated identity Vertex AI access, such as
`roles/aiplatform.user`. See Google's
[setup guide](https://docs.cloud.google.com/vertex-ai/generative-ai/docs/start/quickstart).
For local development, create
[Application Default Credentials](https://docs.cloud.google.com/docs/authentication/set-up-adc-local-dev-environment):

```sh
gcloud auth application-default login
```

Set `GOOGLE_CLOUD_PROJECT` in `.env`. This backend sends Gemini requests through the
configured Cloud project and uses its billing; it does not need `GEMINI_API_KEY`.
On Google Cloud, use the workload's service account through ADC.

To use the Gemini Developer API instead, set `GEMINI_BACKEND=developer` and supply
`GEMINI_API_KEY`. Jeeves does not fall back between the two backends.

| Variable | Purpose / default |
| --- | --- |
| `DISCORD_TOKEN` | Required bot token, without the `Bot ` prefix. |
| `DATABASE_URL` | Required PostgreSQL URL, including any SSL options. |
| `DATABASE_MAX_CONNECTIONS` | Connection pool size; defaults to `5`. |
| `TYPESAFE_API_KEY` | Required for Jev rule classification and evaluation. |
| `TYPESAFE_MODEL` | Optional; defaults to `jev-latest`. |
| `GEMINI_BACKEND` | `vertex` or `developer`. The example uses `vertex`; when unset, the code defaults to `developer`. |
| `GOOGLE_CLOUD_PROJECT` | Required for Vertex AI. |
| `GOOGLE_CLOUD_LOCATION` | Vertex AI location; defaults to `global`. |
| `GEMINI_API_KEY` | Required only for the Developer API backend. |
| `GEMINI_MODEL` | Defaults to `gemini-3.7-flash`; set a model available to your backend/project. |
| `RUST_LOG` | Logging filter; the example uses `info,jeeves=debug`. |

`.env` is loaded at startup and is gitignored.

### Start the bot

The example database URL matches the local Compose service:

```sh
docker compose up -d --wait
cargo run
```

For an existing PostgreSQL or Neon database, set `DATABASE_URL` and skip the Compose
command. Startup applies embedded migrations, registers global slash commands, and
connects to Discord. Ctrl-C and SIGTERM drain queued work within a shutdown deadline.

### PostgreSQL and TLS

```dotenv
# Local Compose database
DATABASE_URL=postgres://jeeves:jeeves@localhost:5432/jeeves?sslmode=disable

# Hosted database
DATABASE_URL=postgres://user:password@db.example.com:5432/jeeves?sslmode=verify-full
```

The bot parses connection settings from the URL. When no SSL mode is specified, it
uses `verify-full`; explicit modes are preserved. For a private CA, append
`&sslrootcert=certs/postgres-ca.pem`. Percent-encode special characters in credentials
and query values. Discord connections and PostgreSQL TLS use rustls with native roots.
The Compose database binds to localhost and uses plaintext for local development.

The consolidated [initial migration](migrations/20260917210000_initial_schema.sql)
creates `messages`, `strikes`, `message_actions`, and `strike_actions`. The database
role needs permission to apply migrations. This is a fresh-install baseline:
installations that used the earlier separate migrations need their migration history
rebased before using it.

## Message context and storage

Jeeves stores messages from readable server channels, keeping the newest **500 per
channel**. PostgreSQL triggers enforce retention, with indexes for guild/channel and
timestamp queries. Edits update existing rows; single and bulk deletes remove stored
messages and their image results. Direct messages and pre-startup history are not
collected. There is no history backfill for events missed while disconnected.

For each new message, Jeeves:

1. Describes supported image attachments with Gemini.
2. Fetches channel history and matching message rules concurrently.
3. Evaluates each rule in its own task while storing the message and image results.
4. Records strikes, evaluates strike rules, and applies the combined moderation result.

History is ordered oldest first and excludes the incoming message, which is passed
separately to evaluators. Storage follows gateway order so later edits and deletes
cannot overtake the original insert. Edits update stored context but do not rerun
message rules. Jeeves skips moderation of its own messages.

Supported attachments are PNG, JPEG, WebP, HEIC, and HEIF, up to **12 MiB each**.
Descriptions and errors are stored in `messages.images` as JSONB. Failed descriptions
do not prevent message storage or rule evaluation. Edits reuse successful descriptions
for unchanged attachments. GIFs, videos, stickers, and linked previews are not described.

For Jev evaluation, message text, channel history, image descriptions, or strike history
are sent to TypeSafe. Image bytes, channel-scope extraction requests, and code-generation
requests go to the configured Gemini backend. Provider-side retention is separate from
Jeeves' 500-message database limit. Rule processing and image work are not durably queued
for replay after a restart.

## JavaScript rules

Gemini generates code automatically for computational rules. Each script is a
synchronous function taking one array and returning an allowed outcome. Message rules
receive up to 500 earlier channel messages followed by the current message:

```js
(messages) => {
  const current = messages.at(-1);
  const repeats = messages.filter(
    message => message.author_id === current.author_id
      && message.content === current.content
      && message.timestamp >= current.timestamp - 30_000
  );
  return repeats.length >= 3 ? "STRIKE" : null;
}
```

Message fields are `id`, `guild_id`, `channel_id`, `author_id`, `content`, `timestamp`,
`edited_timestamp`, and `images`. Discord IDs are strings; timestamps are Unix
milliseconds. `edited_timestamp` may be null. Image entries include attachment ID,
URL, MIME type, description, and description error. Return `"BAN"`, `"KICK"`,
`"STRIKE"`, or `null`.

Strike rules receive that member's earlier strikes across the server, followed by
the new strike exactly once:

```js
(strikes) => strikes.length >= 3 ? "BAN" : null
```

Strike fields are `id`, `guild_id`, `channel_id`, `user_id`, `moderator_id`, `reason`,
`created_at`, `interaction_id`, `source_message_id`, and `source_action_id`. Strike
and Discord IDs are strings; `source_action_id` is an integer or null. Source IDs can
be null, and `created_at` is Unix milliseconds. Return `"BAN"`, `"KICK"`, or `null`;
`"STRIKE"` is rejected to prevent recursion.

Scripts run in fresh QuickJS runtimes with no filesystem, network, environment, or
host API bindings. Limits are one second of execution, 64 MiB of JavaScript memory,
and 64 KiB of source, with at most four scripts running concurrently. Invalid results,
promises, exceptions, and limit failures are logged per rule; successful sibling
rules still apply.

## Development

The reusable `jeeves::typesafe` client supports typed Choice, Score, and Noul questions,
mixed batches, response validation, model listing, and configurable retries and timeouts.
See the [example](examples/typesafe.rs), which loads `.env` and sends a sample evaluation:

```sh
cargo run --example typesafe
```

| Source | Responsibility |
| --- | --- |
| [`src/bot.rs`](src/bot.rs) | Gateway events, guild-count presence, shutdown. |
| [`src/commands.rs`](src/commands.rs) | Slash command registration and routing. |
| [`src/add_action.rs`](src/add_action.rs), [`src/manage_actions.rs`](src/manage_actions.rs) | Rule creation and administration. |
| [`src/messages.rs`](src/messages.rs), [`src/gemini.rs`](src/gemini.rs) | Ordered storage, image descriptions, channel extraction, code generation. |
| [`src/message_actions.rs`](src/message_actions.rs), [`src/strike_actions.rs`](src/strike_actions.rs) | Concurrent rule evaluation and context. |
| [`src/moderation.rs`](src/moderation.rs), [`src/moderation/hierarchy.rs`](src/moderation/hierarchy.rs) | Outcome reconciliation, role checks, Discord actions. |
| [`src/strikes.rs`](src/strikes.rs), [`src/strikes/`](src/strikes/) | Strike persistence, history, and admin controls. |
| [`src/typesafe/`](src/typesafe/), [`src/message_actions/javascript.rs`](src/message_actions/javascript.rs) | Typed Jev client and bounded JavaScript execution. |
| [`src/config.rs`](src/config.rs), [`src/db.rs`](src/db.rs) | Configuration, connection pool, and migrations. |

The gateway currently uses one shard.

Run the regular checks without a live database or API credentials:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Database checks need a disposable database. With the local Compose service running:

```sh
docker compose exec postgres createdb -U jeeves jeeves_test
TEST_DATABASE_URL='postgres://jeeves:jeeves@localhost:5432/jeeves_test?sslmode=disable' \
cargo test --bin jeeves -- --ignored --test-threads=1
```

These checks apply migrations and exercise retention, concurrent writes, image storage,
strike escalation, duplicate prevention, pagination, and scoped removal. Discord and
model calls are mocked; the checks do not issue live moderation actions.

## Deployment and IAM

[CI](.github/workflows/ci.yml) runs formatting, Clippy, unit tests, documentation tests,
and PostgreSQL integration tests on pushes to `main` and pull requests. It uses Rust
1.98.0 and a disposable PostgreSQL 17 service. Tests do not read production secrets.

After checks pass on `main`, CI builds the [container](Dockerfile), pushes it to Artifact
Registry, and deploys its immutable digest to the `jeeves` namespace in the existing
`infrawrench-prod` GKE cluster in `us-east4`. Manual workflow runs on `main` do the same.
Pull requests only run checks.

Google Cloud authentication uses
[Workload Identity Federation](https://github.com/google-github-actions/auth).
Its IAM provider accepts only this repository's numeric repository/owner IDs and the
`main` branch's CI workflow, for push or manual runs. The `jeeves-ci` account can publish
to Jeeves' Artifact Registry repository and discover the cluster. Kubernetes RBAC
limits its deployment and runtime-secret access to the `jeeves` namespace. No
service-account key is stored in GitHub.

The pod uses a separate `jeeves-runtime` identity with Vertex AI access, linked to its
Kubernetes service account through GKE Workload Identity. The
[deployment](deploy/deployment.yaml) runs one non-root replica with a read-only root
filesystem. `Recreate` stops the old bot before starting its replacement, so updates
include a short disconnect. Main-branch deployment runs are serialized.

The repository uses these Actions variables:

| Variable | Value |
| --- | --- |
| `GCP_PROJECT_ID` | Google Cloud project hosting the CI identity. |
| `GCP_WORKLOAD_IDENTITY_PROVIDER` | Full Workload Identity Federation provider resource name. |
| `GCP_SERVICE_ACCOUNT` | CI service account email. |
| `AR_REGISTRY` | Artifact Registry repository URL, without the image name. |
| `GKE_CLUSTER` | Target cluster name. |
| `GKE_REGION` | Cluster and Artifact Registry region. |

Runtime configuration from `.env` is stored as encrypted repository Actions secrets.
[The deployment script](scripts/deploy.py) applies those values to the `jeeves-env`
Kubernetes Secret over stdin, then updates the deployment and waits for its rollout.
Secrets are excluded from the image build context and are never written into image
layers. A configuration checksum triggers a restart when secret values change.
To update runtime configuration and redeploy the current `main` commit:

```sh
gh secret set --repo Infrawrench/Jeeves --env-file .env
gh workflow run ci.yml --repo Infrawrench/Jeeves --ref main
```

`.env`, local Neon/provider state, and temporary cloud credentials are excluded from
Git. The namespace, Kubernetes service account, and CI role binding are defined in
[the bootstrap manifest](deploy/bootstrap.yaml) and provisioned separately from routine
deployments. The bot needs outbound access to Discord, PostgreSQL, TypeSafe, and Google
Cloud; it does not expose an HTTP service or ingress.

With cluster credentials configured, inspect the running deployment with:

```sh
kubectl --namespace=jeeves get pods
kubectl --namespace=jeeves logs deployment/jeeves --tail=50
```

## License

[MIT](LICENSE) — Copyright (c) 2026 Infrawrench LLC.

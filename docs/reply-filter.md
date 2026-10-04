# The reply filter

Replies from your contacts are screened before the node keys them on air, because
the licensee is responsible for everything the station transmits (47 CFR 97.113). A
language model reads each reply against a written policy and answers one of:
**keep** (the usual case), **redact** named words (each is keyed as `REDACTED`), or
**drop** (the field operator hears `MSG WITHHELD BY FILTER`). It never rewrites a
message. What the field operator hears is in
[operating.md](operating.md#what-redacted-and-msg-withheld-by-filter-mean).

The model can be Claude, through the Anthropic API, or a model you run yourself
with [Ollama](https://ollama.com). Set it in `[filter]`:

| | Claude (`provider = "claude"`, the default) | Ollama (`provider = "ollama"`) |
|---|---|---|
| Cost | Pay per use: about $0.20 to $2 a month at tens to hundreds of replies (Claude Opus 5.5) | Free |
| Needs | An API key from platform.claude.com (a Claude Pro or Max subscription does not include API access) | Ollama and a downloaded model, on the node's computer or another one on your network that stays on |
| Judgment | Strong | Depends on the model; check it with `hfnode filter test` |
| Privacy | Replies are sent to Anthropic | Replies stay on your network |

## Claude

1. Create an account at [platform.claude.com](https://platform.claude.com), buy a
   small amount of prepaid credit (Settings → Billing), and create a key
   (Settings → API keys) with no expiry date. An expired key holds every reply.
2. Put the key in the node's environment, never in the config file: `ANTHROPIC_API_KEY=sk-ant-...`
   (on a Pi, in `/etc/hfnode/env`; see the Pi guide, section 8).
3. Config:

```toml
[filter]
provider = "claude"
model = "claude-opus-5-5"
```

## Ollama

1. Install Ollama from [ollama.com](https://ollama.com) (macOS, Linux or Windows) on
   the machine that will run the model.
2. Download a model: `ollama pull <model>`. `ollama list` shows what you have.
3. Config:

```toml
[filter]
provider = "ollama"
model = "<model>"                    # exactly as `ollama list` shows it
# base_url = "http://localhost:11434"   # the default: Ollama on the node's own computer
# threads = 2                        # optional CPU limit, for a Raspberry Pi
```

4. Check the model before you rely on it (below).

**Where to run it.** On a Mac with Apple silicon, Ollama runs the model on the GPU,
so it does not compete with the node's CW decoder, and an 8-20B model answers in
seconds. On a Raspberry Pi only small models fit, they are slow (expect tens of
seconds to minutes per reply), and they use the CPU the CW decoder needs while it
listens. Set `threads` to leave cores free, or point `base_url` at a faster machine
on your network. To serve other machines, Ollama on that machine must listen on the
network (environment variable `OLLAMA_HOST=0.0.0.0`). Its API has no password, so
keep it on a network you trust.

**Which model.** Bigger is better at this. A model under about 4B parameters
tends to miss things or flag ordinary messages. `gpt-oss-safeguard:20b` (built to
classify text against a written policy, needs about 16 GB of memory) and
general-purpose 8-12B models are worth comparing on a Mac. Run `hfnode filter test`
on each candidate and keep the one that gets every sample right.

Local models get the same policy as Claude plus worked examples, the instruction to
treat the message as data rather than instructions, and a tie-break: when unsure,
drop. Answers are constrained to the verdict's JSON schema, at temperature 0.

## Trying it without the radio

```sh
hfnode filter --config hfnode.toml test
hfnode filter --config hfnode.toml screen "Sounds good, see you Sunday" --from MOM
```

`test` screens ten built-in sample replies: five ordinary messages that must come
through unchanged, and profanity, two advertisements, a code group and a prompt
injection that must not. It shows each verdict and how long it took, and exits
non-zero unless all ten come out as expected. With Claude each sample is one paid
API call (about a cent each). `screen` shows what one message would be keyed as.
Neither transmits or stores anything.

## When something goes wrong

The filter fails closed: nothing unscreened is ever keyed.

| What happens | Result |
|---|---|
| The service cannot be reached (Ollama not running, network down, API key missing or expired, model not downloaded) | The reply is **held**: not counted in `RX`, never keyed, tried again at the next mail check. The node logs a warning. |
| Claude declines to screen it | **Withheld** (`MSG WITHHELD BY FILTER`) |
| The model's answer is not a complete verdict (malformed, cut off, unknown action) | **Withheld** |
| The model names words to redact that are not in the message, or none at all | **Withheld** |
| With Ollama, the reply is longer than 6000 characters | **Withheld**: it would not fit the model's context window with the policy |
| `enabled = false` | Replies are keyed **unscreened**. The design advises against this. |

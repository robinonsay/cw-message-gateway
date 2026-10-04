# Texting: Google Voice and iMessage

Besides email, the node can reach a contact by text message, two ways:

- **Google Voice**: texts from a free Google Voice number on the node's Gmail
  account. Works with any phone (iPhone or Android) on a US or Canadian number.
- **iMessage**: iMessages from your own Apple ID, sent by Messages on the Mac the
  node runs on. Only for contacts with Apple devices, and only on a Mac.

Both are optional: leave out `[google_voice]` and `[imessage]` and the node uses
email only, as before.

**None of this has been tried on a real Google Voice account or a real Mac yet.**
How Google Voice forwards texts as mail and how Messages stores them are not
documented by Google or Apple; the node's handling is written from what other
programs and users report, and tested only against made-up copies. Every step
marked *(unverified)* below is something to check the first time. Do the [first-time
checks](#first-time-checks) before relying on either route in the field.

## How TX reaches a contact

A contact needs at least one of `phone`, `imessage` and `address`:

```toml
[[contacts]]
name = "MOM"
phone = "+1 555 123 4567"          # texted from the node's Google Voice number
imessage = "+1 555 123 4567"       # or her Apple ID email; a list is fine
address = "mom@example.com"        # email

[[contacts]]
name = "BOB"
address = "bob@example.com"
```

For each TX the node uses the first of these that works:

1. **iMessage**, if the contact has `imessage` and Messages on this Mac is ready.
2. **Google Voice**, if the contact has `phone` and has texted the node's Google
   Voice number at least once (the node needs a mail from them to learn where to
   send replies).
3. **Email** to `address`. A carrier email-to-SMS address (such as
   `5551234567@vtext.com`) is not used when `phone` is set: most US carriers have
   shut those gateways down.

Google Voice and email both go out through the node's mail server, so the node
tries at most one of them: after a mail server error it does not try the other,
which could deliver the message twice. iMessage falls through to them only when it
failed before anything could have gone out (for example Messages is signed out).

When none of them is possible the field operator hears `FAIL 43 NO ROUTE`. See
[operating.md](operating.md#texts-and-imessages) for what the operator hears.

Every text and iMessage the node sends starts with a tag naming the field callsign
and warning that replies go on the air:

```
W5XXX via HF radio, replies are read on air: RUNNING LATE HOME SUN
```

Change it with `tag` in `[google_voice]` or `[imessage]`; it must contain `{call}`.
Email keeps its `From W5XXX` subject and footer.

`hfnode codes` prints the route of each contact under the code table, by kind only
(no numbers on paper).

## Google Voice

### Setting up the number

All of this is done once, at home, in a browser signed in to the **node's Gmail
account** (the `[email]` account, not your own):

1. Go to voice.google.com and pick a number. Google asks for an existing US phone
   number to verify with; it must not already back another Google Voice account
   *(unverified)*. Your own cell phone is fine for this: Google Voice only uses it
   to verify.
2. In Google Voice settings, under Messages, turn on **Forward messages to email**,
   and leave forwarding to linked numbers off *(unverified names)*.
3. Turn off Google Voice's spam filter, or save each contact's number in the node
   account's Google Contacts, so a first text from a contact is not filtered away
   *(unverified)*.
4. In `hfnode.toml`:

   ```toml
   [email]
   smtp_host = "smtp.gmail.com"
   smtp_port = 465
   imap_host = "imap.gmail.com"
   imap_port = 993                     # needed: other ports have no read timeout
   username = "node@gmail.com"
   from_address = "node@gmail.com"     # must be the Gmail account Google Voice is on
   authserv_id = "mx.google.com"

   [google_voice]
   number = "+1 555 000 1111"          # the node's Google Voice number
   ```

   Give each contact who should get texts a `phone`, written with its country code.

5. Have each such contact **text the node's Google Voice number once** (anything,
   "hello" is fine). Until they do, TX to them has no Google Voice route. That first
   text is a real message: it is read out on the next `RX`.
6. Check with `hfnode messages check` (below): each contact should show
   `Google Voice (learned ...)`.

**Keep the number.** Google takes back a Google Voice number that is not used for
some months *(unverified: about 3)*. `hfnode messages check` warns when the node
has sent nothing from it for 60 days; send a text now and then:

```sh
hfnode messages --config C send --via google-voice MOM "Testing the radio node"
```

### What the node takes from it

Google Voice forwards each text to the node's Gmail. The node:

- takes only mail from Google Voice's own address that passes Gmail's DKIM check,
  from a contact's `phone`, and only in the one-to-one format; a group text or
  anything it does not recognise is not read out *(group format unverified)*;
- strips Google Voice's footer; a text it cannot cut cleanly is not read out:
  the operator hears `FM MOM TEXT NOT READABLE SEE NODE LOG` instead, so they know
  something came;
- shortens a phone's reaction ("Liked "RUNNING LATE"") to `LIKED YOUR MSG`;
- learns the contact's reply address from the mail. Opening the mail in Gmail is
  fine for that, but an opened mail is no longer read out on `RX` (mark it unread to
  get it read). **Do not archive or delete** Google Voice mail before
  `messages check` shows the contact learned.

Replies from a number that is not a contact are never read out.

### Group texts

Not tested yet. Until it has been, do not rely on how the node treats a group
text that includes the node's number. To test it:

1. The contact texts the node's number one-to-one: `messages check` shows `would
   learn` for them.
2. Start a group text with the node's number, the contact, and a second phone you
   control: `messages check` should show it `not learned`, or not at all.
3. `messages send` to the contact reaches only the contact.

## iMessage (Mac only)

The node sends iMessages through Messages on its Mac, as your Apple ID, and reads
the contact's replies from Messages' database. It reads a reply only if all of
these hold:

- it is from one of the contact's `imessage` handles, in a one-to-one chat (never
  a group chat), over iMessage (not SMS or RCS);
- it was sent within `reply_hours` (default 48) after the node's last iMessage TX
  to that contact. Your ordinary conversations with that person outside those
  windows are never read, and nothing from anyone else is;
- it has been there for 2.5 minutes, since a sender can unsend for 2 minutes.

So a contact can answer a radio iMessage for 2 days. After that, or to start a
conversation, they text the Google Voice number instead.

### Setting it up

iMessage works only when the node is started by `hfnode.command` in **Terminal**
(macos-setup.md section 7). The permissions it needs are Terminal's; the launchd
agent cannot use them, and under it the node logs `iMessage not available: the
node was started by the launchd agent` and uses the other routes.

1. Sign in to iMessage in Messages on the Mac, with the Apple ID your contacts
   know, and check you can send one by hand.
2. Stop the node (Ctrl-C in its window).
3. Give Terminal **Full Disk Access**: System Settings > Privacy & Security > Full
   Disk Access, turn on Terminal, and accept **Quit & Reopen**. This closes any
   node running in Terminal.
4. Add to `hfnode.toml`:

   ```toml
   [imessage]
   # defaults: db = "~/Library/Messages/chat.db", poll_secs = 60, reply_hours = 48
   ```

   and give the contacts their `imessage` handle (the phone number or email their
   iMessage uses).
5. In **Terminal.app** (not iTerm or another terminal), run:

   ```sh
   D="$HOME/Library/Application Support/hfnode"
   hfnode messages --config "$D/hfnode.toml" check
   hfnode messages --config "$D/hfnode.toml" send --via imessage MOM "Testing the radio node"
   ```

   The first time, macOS asks "Terminal wants access to control Messages":
   answer **Allow** *(unverified wording)*. If you answered Don't Allow, turn it on
   under Privacy & Security > Automation > Terminal > Messages, or reset it with
   `tccutil reset AppleEvents com.apple.Terminal` and try again.
6. Start the node with `hfnode.command` and look for `iMessage ready` in its log.

Check again after a macOS update and after rebuilding `hfnode`.

**What this gives Terminal.** With Full Disk Access, anything run in Terminal can
read all your files, Messages, Mail and Safari included, and with the Automation
grant it can send iMessages as you. Never add `/bin/sh`, `/bin/bash`, `/bin/zsh`,
`osascript` or the `hfnode` binary to Full Disk Access yourself, and answer Don't
Allow if `sh`, `bash` or `osascript` asks to control Messages: those grants would
apply to every script on the Mac.

### Sending

After the field operator's `OK`, Messages gets the iMessage and the node watches
Messages' database for up to 30 s for it to go out. It keys `SENT` once Messages
shows it sent; `FAIL GATEWAY` when it shows an error or nothing in that time (the
iMessage may still arrive later). Some failures turn iMessage off until it next
checks (every 10 minutes): Messages signed out, or the Automation permission
refused. A TX then goes by Google Voice or email.

## Checking it: `hfnode messages`

```sh
hfnode messages --config C check [--since HOURS] [--save-raw DIR] [--dump ROWID]
hfnode messages --config C send [--via imessage|google-voice|email] [--call W5XXX] NAME TEXT
```

`messages check` changes nothing: it reads the mailbox without marking anything
read and writes nothing in `state_dir`. Run it with the node running. It shows:

- **Routes**: how TX would reach each contact right now, or `NO ROUTE` and why.
- **Google Voice**: each phone contact's reply address and when it was learned,
  when the node last sent a text, and the newest Google Voice mails with what the
  node would make of each (`would learn`, `not learned`).
- **Mailbox**: the last 2 days of mail as the node's RX would take it: which
  contact, the text up to where the footer is cut, and any reaction.
- **iMessage**: whether Messages is ready, each handle as Messages knows it, the
  reply windows, and the contacts' recent iMessages with what the node would do
  with each. Text is shown only for messages inside a reply window; messages from
  anyone else are only counted.

`--save-raw DIR` saves the Google Voice mails as `.eml` files (readable only by
you) for checking the format. They hold phone numbers, reply tokens and private
texts: never commit them as they are (see [Privacy](#privacy)). `--dump ROWID`
prints one contact message's stored text bytes, for checking the decoder.

`messages send` really sends, by the route a TX would take (or only the one named
with `--via`), tagged as from `--call` (default: the first `field_calls`). An
iMessage opens a reply window, as a TX does. It exits 0 when sent, 1 when the route
failed, 2 when there is no route.

## Before a trip

- With the node running, `hfnode messages check`: every contact shows a route.
- Every contact reached by Google Voice has texted the node's number once (their
  text is read out on the first `RX`).
- One real test per route, with `messages send`, and the contact confirms it
  arrived.
- `messages check` cannot tell a lapsed Google Voice number from a quiet one; the
  test send can.
- Tell contacts what [operating.md](operating.md#texts-and-imessages) says to tell
  them.

## Privacy

- The node never logs the text of an incoming message, and never logs the number
  or handle of anyone who is not a contact. iMessage log lines carry the contact's
  name and Messages' row number only.
- Google Voice reply addresses are not logged on send.
- With `filter.provider = "claude"` replies go to Anthropic for screening, as email
  replies do; with Ollama they stay on your network ([reply-filter.md](reply-filter.md)).
- Captured mail used as test data in this repository must be redacted first:
  555 numbers for every phone number, a made-up token in each Google Voice address,
  example.com for every email address, made-up text, and no other headers than the
  ones a test needs.

## Files in `state_dir`

| File | What it is |
|---|---|
| `google_voice.json` | Each phone contact's learned Google Voice reply address, and how far the node has looked through the mailbox. |
| `imessage.json` | Where in Messages' database the node has read to. It belongs to one Mac: after moving the node, expect one `rescanning` warning. |
| `imessage_windows.json` | Each contact's reply window, and the node's recent iMessage sends. |
| `*.lock` | Taken while one of the above is written, so the node and `messages send` do not overwrite each other. |

## First-time checks

Tick these off the first time, with `hfnode messages --config C check --save-raw DIR` for the
Google Voice ones.

Google Voice:

- [ ] A one-to-one text arrives in the inbox unread; the saved mail has a plain-text
  part, the footer lines the node cuts, and a `Message-ID`.
- [ ] Its topmost `Authentication-Results` header is from `mx.google.com` with
  `dkim=pass`. If there is none, Google Voice texts cannot be authenticated and are
  never read out.
- [ ] `messages check` shows the contact as `would learn`, and after the node's next
  mail check, `learned`.
- [ ] Opening the mail in Gmail does not stop the address being learned.
- [ ] Multi-line text, emoji, a picture, a picture with a caption, two texts in a
  row: each is read out or gets the not-readable notice, never footer text.
- [ ] A voicemail or missed call to the number is not read out.
- [ ] The group text test above.
- [ ] `messages send --via google-voice`: the phone gets only the text (no subject,
  no signature), from the node's number; a 200-character text arrives whole.
- [ ] An iPhone reaction to the node's text (Like, a custom emoji, removing one) is
  read as `LIKED YOUR MSG` and so on, and a removal is not read.
- [ ] Texts are read in the order they were sent.

Mac (note the macOS version, `sw_vers -productVersion`):

- [ ] Without Full Disk Access `messages check` says macOS refused access; with it,
  it reads the database.
- [ ] Each contact's handle shows as `found in Messages (iMessage)`.
- [ ] A one-to-one test message from the contact shows as `would take: ...` inside a
  window, a family group message as `skipped: group ...`.
- [ ] Two test messages from the contact, one short and one over 128 characters
  with an emoji, are shown with their exact text.
- [ ] An unsent message is skipped. Note what an edited one gives: the node reads
  the text as it stands 2.5 minutes after it was sent.
- [ ] `messages send --via imessage` prompts for Automation once, then sends; with
  Messages signed out it reports Messages is not ready.
- [ ] A send to a number not on iMessage ends in `FAIL GATEWAY`, and how long it
  takes.
- [ ] Under the launchd agent the log says iMessage is not available, and Google
  Voice and email still work.
- [ ] With Messages quit, a reply still arrives in the database and is read.
- [ ] End to end: an iMessage TX to the contact, their reply read on `RX`; a reply
  after `reply_hours` is not.

# Operating guide (field operator)

How to use the node from the field: what to carry, what to send, and what the node
sends back. Examples use field callsign `W5XXX` and node callsign `N0CALL`; the node
always ends with `DE <its callsign> K`.

## Before you leave

- **Frequency and windows.** Know the node's frequency and its listening windows. By
  default the node listens for the **first 10 minutes of every hour, UTC**
  (`[schedule]` in the node's config). Outside a window it does not decode or
  transmit at all.
- **Contacts.** Know the contact names configured at home (for example `MOM`,
  `BOB`). You can only send to those names.
- **Where you will be, for weather.** `WX` gives the forecast for the place you
  name: your grid square, or one of the node's numbered presets (printed under the
  code table). Look up the 6-character grid square of each place you will key from
  before you go (GPS and ham apps show it), and write it on the table.
- **The code table.** Print a fresh one (below) and check that its first line is
  above the last code you used.
- **Tell your contacts** to reply to the node's email (or text) with short, plain
  messages. Quoted text and signatures are stripped, and only replies from the
  contact's configured address or phone number are kept.

## The code table

Print it at home on the node:

```sh
sudo -u hfnode hfnode codes --config /etc/hfnode/hfnode.toml --count 100
```

It looks like this (codes shown in two groups of four letters, three columns, read
down each column):

```
N0CALL code table, sequence 43-142
Use each line once, in order; skipping lines is fine. Two lines per message.

   43  WBNF HJGC         77  ....              111  ....
   44  ....              78  ....              112  ....
```

Rules:

- **Every transaction uses two lines**: one to open it, a later one to commit it.
  Normally these are the next two unused lines.
- **Use lines in order and cross each one off as you use it.** Never go back to a
  lower number: the node only accepts a sequence number higher than the last one it
  acted on, so lower lines are dead.
- **Skipping lines is fine.** If you are unsure whether a line was used, skip to the
  next one.
- **The table is a password.** Anyone holding it can send messages as you until the
  codes are used. Keep it on you. If it is lost, at home: stop the node, move
  `/etc/hfnode/node.key` aside, create a new key with `hfnode keygen`, restart, and
  print a new table. All old codes stop working.

When sending a code you can send the eight letters together (`WBNFHJGC`) or in the
two printed groups (`WBNF HJGC`). Sequence numbers and codes must be copied exactly
by the node; there is no error correction on them.

## How an exchange works

1. You **open**: callsign, sequence number, code, and a request (`TX`, `RX` or `WX`).
2. The node **reads back** what it understood, ending with `?`.
3. If the read-back is right, you **commit** with `OK` and the next line's number and
   code. If it is wrong, send `NO`.
4. The node acts and replies.

Nothing is sent, read out or looked up until the commit. End every transmission
with `K`. The node treats your transmission as finished after about 3 seconds of
silence, so do not pause longer than that in the middle of one.

## Formats

| You send | Meaning | Node replies |
|---|---|---|
| `W5XXX 42 KRTPQMLD TX MOM RUNNING LATE HOME SUN K` | Open: send text to a contact | `R 42 TX MOM RUNNING LATE HOME SUN ? DE N0CALL K` |
| `W5XXX 44 <code> RX K` | Open: read new messages | `R 44 3 MSGS ? DE N0CALL K` (`1 MSG`, `0 MSGS`) |
| `W5XXX 46 <code> WX DL89IG K` | Open: forecast for a grid square (4 or 6 characters) | `R 46 WX DL89IG ? DE N0CALL K` |
| `W5XXX 46 <code> WX 1 K` | Open: forecast for preset 1 | `R 46 WX 1 DL89IG ? DE N0CALL K` |
| `W5XXX 46 <code> WX K` | Open: forecast for the node's default grid square | `R 46 WX DL89 ? DE N0CALL K` |
| `OK 43 WBNFHJGC K` | Commit the pending transaction | depends on the request, see below |
| `NO K` | Abort the pending transaction | `R NO DE N0CALL K` |
| `AGN K` | Repeat the node's last transmission | the last transmission again |
| `AGN B K` | Repeat chunk B of the last transmission | `<chunk B text> = B DE N0CALL K` |

The commit's sequence number must be higher than the open's (normally the next
line).

Your callsign, the words `TX`, `RX`, `WX`, and contact names are matched loosely,
so a slightly garbled one still works; the read-back shows what the node made of it.
Sequence numbers and codes are not.

### TX: send a text or email

```
You:   W5XXX 42 KRTPQMLD TX MOM RUNNING LATE HOME SUN K
Node:  R 42 TX MOM RUNNING LATE HOME SUN ? DE N0CALL K
You:   OK 43 WBNFHJGC K
Node:  SENT 43 DE N0CALL K
```

`SENT` means the node handed the message to its mail server. The contact receives an
email (or a text, through their carrier's email-to-SMS address) from the node with
the subject `From W5XXX`.

**Check the read-back word by word.** Words the node could not decode are left out,
and only characters that exist in Morse are sent on. If anything is wrong, send
`NO K` and start again with the next two lines.

If the node replies `FAIL 43 GATEWAY DE N0CALL K`, the codes were accepted and are
used up, but the message could not be sent (mail server problem). Try again later
with new lines.

### RX: read messages

```
You:   W5XXX 44 <code 44> RX K
Node:  R 44 2 MSGS ? DE N0CALL K
You:   OK 45 <code 45> K
Node:  NR 1 FM MOM DRIVE SAFE CALL WHEN YOU CAN NR 2 FM BOB THE = A
       GAME WAS POSTPONED TO NEXT SATURDAY AT NOON = B DE N0CALL K
```

- Each message starts `NR <n> FM <contact>`.
- Long readouts are sent in **chunks** of up to about 60 characters. Each chunk ends
  with `=` (BT) and its letter: `= A`, `= B`, and so on. The node pauses about 2
  seconds between chunks. The last chunk ends `DE N0CALL K`.
- If no messages are waiting, the reply after `OK` is `R 45 NIL DE N0CALL K`. If the
  read-back already says `0 MSGS`, you can send `NO K` instead of committing.
- At most **5 messages** are read per `RX`. If more are waiting, the text ends
  `<n> MORE`; do another `RX` with the next two lines.
- Messages are marked as read once they are sent. A second `RX` will not send them
  again; use `AGN` (below) if you missed part of them.
- A very long readout is cut off after 26 chunks (`A` to `Z`) and ends with `MORE`.

### WX: weather

```
You:   W5XXX 46 <code 46> WX DL89IG K
Node:  R 46 WX DL89IG ? DE N0CALL K
You:   OK 47 <code 47> K
Node:  WX DL89IG ALERT HEAT ADVISORY TNGT MSTLY CLEAR LO 58 WIND W = A
       5 MPH SAT SUNNY HI 97 WIND SW 10 TO 15 MPH = B DE N0CALL K
```

Name the place you want the forecast for after `WX`:

- **Your grid square**, such as `WX DL89IG`. Use the 6-character square. A
  4-character square (`WX DL89`) is about 110 by 190 km and the forecast is for its
  centre, which can be far from you and at a very different height. A square sent
  as two words (`DL89 IG`) is joined back together.
- **A preset**, such as `WX 1`. Presets are the node's numbered places
  (`[[weather.presets]]` in its config), printed under the code table. Send the
  digits in full, not as cut numbers.
- **Nothing**: `WX` alone gives the forecast for the node's default grid square
  (`weather.default_grid`).

**Check the read-back.** It always names the grid square the forecast will be for,
after the preset number if you sent one. If it is not the place you meant, send
`NO K`. The forecast starts with the same grid square.

The forecast comes from the US National Weather Service, so it covers the US only
(the states and territories). It starts with any active alerts (`ALERT` and the
alert's name, or `ALERTS UNAVBL` if they could not be checked), then the next
periods (`TNGT`, `SAT`, ...) with the sky, the high or low in °F, and the wind. It
is chunked like an `RX` readout.

- `FAIL 47 WX NO COVERAGE DE N0CALL K`: the NWS has no forecast for that grid
  square (outside the US, or at sea). Asking again will not help.
- `FAIL 47 WX DE N0CALL K`: the forecast could not be fetched (for example the
  node's internet connection is down). Try again later.

Either way those codes are used up. A grid square that is not a valid locator, or a
preset number the node does not have, gets silence.

### NO: abort

Send `NO K` after a wrong read-back. The node replies `R NO DE N0CALL K` and forgets
the request. Cross off the open line and start over with the next two lines.

A pending request is also forgotten if you open a new one on fresh lines (the node
reads back the new request), or if you do not commit within **10 minutes** of the
read-back. A late `OK` then gets silence; start over with new lines.

### AGN: repeat

- `AGN K` repeats the node's whole last transmission (a read-back, a `SENT`, or a
  full readout).
- `AGN B K` repeats only chunk `B` of the last readout.
- `AGN` works for about **10 minutes** after the node's last transmission. After
  that, or if there is no such chunk, the node stays silent.
- `AGN` uses no codes.

## Silence

The node never answers anything it cannot decode or authenticate. There is no
"error" reply. Silence means one of:

- the node did not copy you well enough (most likely);
- a sequence number or code was wrong, or the line was already used;
- a contact name, weather preset number or grid square the node does not know or
  that is not valid (check it against your table);
- you are outside a listening window, or not close enough to the node's frequency;
- the node measured a high SWR earlier in this window and has stopped transmitting
  until the next window;
- the node or its radio is down.

**Retries are free.** Send exactly the same transmission again with the same line;
it costs no new codes:

- Repeating an open the node already has makes it repeat the same read-back.
- Repeating an `OK` the node already acted on makes it repeat the same reply
  (`SENT 43` again). The message is not sent twice.

So if you sent `OK` and heard nothing, repeat the `OK`, not the open. If repeated
tries get no answer, wait for the next window. If you are not sure what the node
did, skipping to fresh lines is always safe.

## Timing and listening windows

- With the default schedule the node listens from minute 00 to minute 10 of every
  hour, UTC. At the start of each window it runs its antenna tuner, which transmits
  a carrier for a few seconds. Wait until that is done before calling.
- A transaction that is open when the window ends stays open: the node keeps
  listening until you commit, abort, or the 10 minutes run out.
- Wait for the node's reply before sending again. The reply starts a few seconds
  after you stop (it waits for about 3 seconds of silence first).

## Sending tips

- Send on the agreed frequency. The node's decoder listens in a narrow window
  (150 Hz by default) around its CW pitch, so a signal more than a few tens of hertz
  off frequency may not be decoded at all.
- Keep your speed steady, around 15 to 20 wpm, and leave clear spaces between words.
  The decoder follows your speed, but it handles even, well-spaced sending best.
- Stick to letters, numbers and simple punctuation. Characters Morse does not have
  are dropped.

## What REDACTED and MSG WITHHELD BY FILTER mean

Replies from your contacts are written by people who are not licensed, and the node's
licensee is responsible for everything it transmits. Before a reply can be read over
the air, a compliance filter (an AI model with a stated policy based on FCC Part 97
rules) screens it. The filter never rewrites or summarises a message. It can only:

- **Pass it unchanged** (the normal case).
- **Remove specific words or phrases.** Each one is replaced by the word `REDACTED`.
  Everything else in the message is exactly as written. Example:
  `NR 1 FM BOB SEE YOU SUN REDACTED TRAFFIC`.
- **Withhold the whole message.** You receive `MSG WITHHELD BY FILTER` in place of the
  text, for example for spam or a message that is mostly unfit to transmit. The
  node also withholds a message if the filter's answer could not be applied exactly,
  rather than guess.

A reply that has not been screened yet (for example because the filter service was
unreachable) is not counted in `RX` and is never transmitted. It will be read out
once it has been screened.

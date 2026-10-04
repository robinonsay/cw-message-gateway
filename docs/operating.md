# Operating guide (field operator)

How to use the node from the field: what to carry, what to send, and what the node
sends back. Examples use field callsign `W5XXX` and node callsign `N0CALL`; every
reply ends with `DE N0CALL K`. The node also sends `DE N0CALL` on its own after its
tuning carrier when it starts listening (at start-up, or at the start of a
listening window), between two chunks of a long readout, and before a reply that
would otherwise end more than 8 minutes after its last `DE N0CALL` (the read-back
of a long `TX`, say): that is its station identification, not part of the text.

## Before you leave

- **Frequency.** Know the node's frequency. By default the node listens **all the
  time**. If it has been set to listen only in windows (`[schedule] always = false`
  in the node's config, for example the first 10 minutes of every hour, UTC), know
  them too: outside a window it does not decode or transmit at all, except while it
  is still finishing a transaction you started in one, when it answers whatever it
  would in a window (see [Timing](#timing)). Start new requests inside a window.
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
Use each line once, in order; skipping lines is fine.
Two lines per message (open, OK), and one more for each NO or AGN.

   43  WBNF HJGC         77  ....              111  ....
   44  ....              78  ....              112  ....
```

Rules:

- **Every transaction uses two lines**: one to open it, a later one to commit it.
  Normally these are the next two unused lines.
- **`NO` and `AGN` use a line each too**, the next unused one, like an `OK`. An
  `OK` sent after them goes on a line above theirs. A `NO` or `AGN` without a line,
  or on a line already used, gets silence.
- **Use lines in order and cross each one off as you use it**, whether or not the
  node answered. Never go back to a lower number: the node only accepts a sequence
  number higher than the last one used, so lower lines are dead.
- **Skipping lines is fine.** If you are unsure whether a line was used, skip to the
  next one.
- **The table is a password.** Anyone holding it can send messages as you until the
  codes are used. Keep it on you. If it is lost, at home: stop the node, move the
  key aside, create a new one for the `hfnode` user, start the node, and print a
  new table. All old codes stop working.

  ```sh
  sudo systemctl stop hfnode
  sudo mv /etc/hfnode/node.key /etc/hfnode/node.key.lost
  sudo hfnode keygen --out /etc/hfnode/node.key
  sudo chown hfnode:hfnode /etc/hfnode/node.key
  sudo systemctl start hfnode
  sudo -u hfnode hfnode codes --config /etc/hfnode/hfnode.toml --count 100
  ```

  Back the new key up offline, as when you first made it.

When sending a code you can send the eight letters together (`WBNFHJGC`) or in the
two printed groups (`WBNF HJGC`). Sequence numbers and codes must be copied exactly
by the node; there is no error correction on them.

## How an exchange works

1. You **open**: callsign, sequence number, code, and a request (`TX`, `RX` or `WX`).
2. The node **reads back** what it understood, ending with `?`.
3. If the read-back is right, you **commit** with `OK` and the next line's number and
   code. If it is wrong, send `NO` with the next line's number and code.
4. The node acts and replies.

Nothing is sent, read out or looked up until the commit. End every transmission
with `K` or `KN` (`KN` keyed as one character works too, and so does `AR K`). Only
the last over counts: a message whose last word is `K`, `KN`, `AR` or `SK` needs the
over after it (`TX MOM BRING VITAMIN K K` sends `BRING VITAMIN K`), and one `AR`
just before the over is taken as the end-of-message sign, so to end on the word `AR`
send `AR AR K`. The read-back shows what the node kept. The node treats your
transmission as finished after about 3 seconds of silence, so do not pause longer
than that in the middle of one.

## Formats

| You send | Meaning | Node replies |
|---|---|---|
| `W5XXX 42 KRTPQMLD TX MOM RUNNING LATE HOME SUN K` | Open: send text to a contact | `R 42 TX MOM RUNNING LATE HOME SUN ? DE N0CALL K` |
| `W5XXX 44 <code> RX K` | Open: read new messages | `R 44 3 MSGS ? DE N0CALL K` (`1 MSG`, `0 MSGS`) |
| `W5XXX 46 <code> WX DL89IG K` | Open: forecast for a grid square (4 or 6 characters) | `R 46 WX DL89IG ? DE N0CALL K` |
| `W5XXX 46 <code> WX 1 K` | Open: forecast for preset 1 | `R 46 WX 1 DL89IG ? DE N0CALL K` |
| `W5XXX 46 <code> WX K` | Open: forecast for the last place you confirmed (at first the node's default) | `R 46 WX DL89IG ? DE N0CALL K` |
| `OK 43 WBNFHJGC K` | Commit the pending transaction | depends on the request, see below |
| `NO 43 <code> K` | Abort the pending transaction | `R NO DE N0CALL K` |
| `AGN 44 <code> K` | Repeat the node's last transmission | the last transmission again |
| `AGN 44 <code> B K` | Repeat chunk B of the last transmission | `<chunk B text> = B DE N0CALL K` |
| `AGN 44 <code> K K` | Repeat chunk K (the second `K` is the over) | `<chunk K text> = K DE N0CALL K` |

The commit's sequence number must be higher than the open's and than any line used
since (normally the next line). `NO` and `AGN` take the next unused line, as `OK`
does.

Your callsign, the words `TX`, `RX`, `WX`, and contact names are matched loosely,
so a slightly garbled one still works; the read-back shows what the node made of it.
`OK`, `NO`, `AGN`, sequence numbers and codes are not.

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
`NO` on the next line (`NO 43 <code 43> K`) and start again on the two lines after
it.

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
  read-back already says `0 MSGS`, there is no need to commit: let it lapse (it is
  forgotten after 10 minutes and costs no more lines), or send `NO` on the next
  line.
- At most **5 messages** are read per `RX`, fewer if they would not fit in 26
  chunks (`A` to `Z`). If more are waiting, the readout ends `<n> MORE`; do
  another `RX` with the next two lines. The read-back counts every message waiting,
  so it can say more than 5.
- Messages are marked as read once they are sent. A second `RX` will not send them
  again; use `AGN` (below) if you missed part of them.
- A single message too long for 26 chunks on its own is cut off and ends
  `TRUNCATED`. It counts as read: the rest of it cannot be had over the air.
- A readout longer than about 7½ minutes has `DE N0CALL` on its own between two
  chunks (with the usual pause on both sides, no letter, no `K`). It is the node's
  station identification, not part of the text: copy around it. Chunk letters are
  unaffected, and `AGN` for the whole readout may place it between different
  chunks.
- The 10 minutes in which `AGN` or a repeated `OK` gets the readout again count from
  when the node heard your `OK`, not from the end of the readout. Ask for anything
  you missed as soon as it ends: a readout that takes more than 10 minutes to send
  cannot be repeated at all.

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
  centre, which can be far from you and at a very different height. Send the
  square as one word; if a long gap splits it in two, the node usually joins it
  back together (the read-back shows what it made of it).
- **A preset**, such as `WX 1`. Presets are the node's numbered places
  (`[[weather.presets]]` in its config), printed under the code table. Send the
  digits in full, not as cut numbers.
- **Nothing**: `WX` alone gives the forecast for the last place you asked for and
  confirmed with `OK` (a grid square or a preset), which the node keeps across
  restarts. Until you have done that once, it is the node's default grid square
  (`weather.default_grid`). So once you have sent `WX DL89IG` from camp, a plain
  `WX` gets camp's weather; after you move, name the new place once. A place the
  NWS has no forecast for is not remembered.

**Check the read-back.** It always names the grid square the forecast will be for,
after the preset number if you sent one. If it is not the place you meant, send
`NO` on the next line. The forecast starts with the same grid square.

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

After a wrong read-back, send `NO` with the next unused line's number and code:

```
You:   W5XXX 42 KRTPQMLD TX MOM RUNNING LATE HOME SUN K
Node:  R 42 TX MOM RUNNING LATE HOME ? DE N0CALL K
You:   NO 43 WBNFHJGC K
Node:  R NO DE N0CALL K
```

The node forgets the request. Cross off both lines and start over on the next two.

- A `NO` without a line and its code (`NO K`) is ignored, so nobody who heard your
  exchange can cancel it.
- If you miss the `R NO`, send exactly the same `NO` again: it is answered again,
  free, up to 3 times within 10 minutes, until you use a later line. If the node
  uses listening windows, this works only inside the window: past its end the node
  stops listening once it has sent `R NO`, so a repeat gets silence. If no `R NO`
  comes back, nothing is sent anyway: a request is only acted on after your `OK`.
- A `NO` when nothing is pending (it timed out, or your `OK` already went through)
  gets silence and still uses its line. If you sent `OK` and missed the result,
  repeat the `OK` instead.

A pending request is also forgotten if you open a new one on fresh lines (the node
reads back the new request), or if you do not commit within **10 minutes** of the
first read-back (repeating the open does not restart the 10 minutes). A late `OK`
then gets silence; start over with new lines.

### AGN: repeat

- `AGN 44 <code 44> K` repeats the node's whole last transmission (a read-back, a
  `SENT`, or a full readout).
- `AGN 44 <code 44> B K` repeats only chunk `B` of the last readout. For chunk `K`
  send `AGN 44 <code 44> K K`: the second `K` is the over.
- Like `NO`, `AGN` takes the next unused line and its code, and uses that line even
  when there is nothing to repeat. A bare `AGN K` is ignored.
- After an `AGN` that repeated a read-back, the `OK` goes on the line after the
  `AGN`'s (open 42, `AGN 43`, `OK 44`). Repeating the open instead costs no line.
- If you miss the repeat, send exactly the same `AGN` again (same line, same
  letter): it is answered again, free, up to 3 times within 10 minutes, until you
  use a later line.
- `AGN` works for **10 minutes** after the node's last reply (a repeat sent for an
  `AGN` does not count), counted from when it heard what it was replying to. After
  that, or if there is no such chunk, the node stays silent and the line is used
  all the same.

## Silence

The node never answers anything it cannot decode or authenticate. There is no
"error" reply. Silence means one of:

- the node did not copy you well enough (most likely);
- a sequence number or code was wrong, or the line was already used (also a `NO`
  or `AGN` sent without one);
- a contact name, weather preset number or grid square the node does not know or
  that is not valid (check it against your table);
- you are not close enough to the node's frequency, or outside a listening window
  if the node uses them (past a window's end it listens on only while it is still
  finishing a transaction, see [Timing](#timing));
- the node measured a high SWR, or no output, or its tuner could not match the
  antenna, and it has stopped transmitting until it next tunes: within an hour by
  default (`schedule.retune_minutes`), or at the next window. If you heard its
  tuning carrier at start-up or at a window's start but no `DE N0CALL` after it, the
  tune failed;
- the node has stopped transmitting after a radio fault: it then stays silent until
  it is cleared at home (the owner is emailed when this happens, if an alert
  address is set);
- a thunderstorm is forecast or warned at the node, or the node could not get a
  forecast (storm stand-down: it keeps listening but will not transmit; see below);
- the node or its radio is down.

**Retries are free.** Send exactly the same transmission again with the same line;
it costs no new codes:

- Repeating an open the node already has makes it repeat the same read-back.
- Repeating an `OK` the node already acted on makes it repeat the same reply
  (`SENT 43` again, or the whole readout), up to 3 times and for 10 minutes after
  the node acted on it, also after a listening window has ended. The message is
  not sent twice and the forecast is not fetched again. Once you open a new
  transaction, the old `OK` gets silence.
- Repeating a `NO` or `AGN` exactly (same line, code and letter) gets the same
  answer, also free.

So if you sent `OK` and heard nothing, repeat the `OK`, not the open, within 10
minutes (with windows, the node keeps listening for it past the end of the
window). If repeated tries get no answer, try again in an hour (or at the next
window, if the node uses them). If you are not sure what the node did, skipping to
fresh lines is always safe for your codes, but if the node did act on your `OK`,
the message is sent a second time.

### Storm stand-down

The node will not tune or transmit while the National Weather Service hourly forecast
for the node's own location (`[storm]` in its config) mentions thunder within the next
`lookahead_hours` (default 2), or an active alert there mentions thunder, lightning or
a tornado. It holds for `clear_minutes` (default 30) after the last thunder, and also
whenever it cannot get a forecast. A transmission under way stops and the radio
returns to receive. It keeps decoding, so try again later with fresh lines. If the
stand-down began after your `OK`, the message may already have gone without a
`SENT`, and repeats of that `OK` get no answer while it lasts (they also count
against the usual repeat limits). Check with the recipient, or with an `RX`, before
sending it again. This is about the weather at home, not where you are: check the
forecast for the node before a trip in storm season.

## Timing

- By default the node listens all the time, so you can call whenever you like.
  When it starts up it runs its antenna tuner, a carrier of a few seconds, and then
  sends `DE N0CALL`.
- When the node last tuned more than an hour ago (`schedule.retune_minutes`), it
  runs its antenna tuner before its reply: you hear a few seconds of carrier, then
  the read-back, which identifies it. Otherwise it tunes only when it starts up.
- If the node is set to listen in windows, it tunes at the start of each window,
  which transmits a carrier for a few seconds, and then sends `DE N0CALL`: wait for
  that before calling. If you hear the carrier but no `DE N0CALL`, the tune failed
  and the node may stay silent until its next tune.
- Past the end of a window the node keeps listening while it is finishing a
  transaction you started in it: while it is pending (until you commit, abort, or
  the 10 minutes run out), and then for up to 10 minutes after the result, so that
  a repeated `OK` or an `AGN` still gets it (not if it cannot transmit). Meanwhile
  it answers anything it would in a window, a new request included, which keeps it
  listening longer. Once nothing holds it, or after `R NO`, it is silent until the
  next window. Start new requests inside a window.
- Wait for the node's reply before sending again. The reply starts a few seconds
  after you stop (it waits for about 3 seconds of silence first, and a tune adds a
  few more).

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
  or was not a clear verdict, rather than guess.

A reply that has not been screened yet (for example because the filter service was
unreachable) is not counted in `RX` and is never transmitted. It will be read out
once it has been screened.

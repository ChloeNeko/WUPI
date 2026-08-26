# Spellcheck word lists — sources

`dict-en.txt` — SCOWL 80 (Spell Checker Oriented Word Lists), Kevin
Atkinson. Prebuilt "size 80, US spelling, max_variant 2" list generated
from http://app.aspell.net/create (mirror: BartMassey/wordlists
`scowl-80.txt.gz`), filtered to lowercase entries only (capitalized
lines are proper nouns). ~284k words.

SCOWL license (Kevin Atkinson): "Permission to use, copy, modify,
distribute and sell these word lists, the associated scripts, the
output created from the scripts, and its documentation for any purpose
is hereby granted without fee, provided that the above copyright notice
appears in all copies..." Full notice: wordlist.aspell.net.

`common-en.txt` — google-10000-english (first20hours), the 10k most
frequent English words, one per line in rank order. Used to rank
suggestion candidates and to patch gaps in the main list. MIT licensed.

Neither file is modified after download except: lowercase-entry filter
+ sort/uniq on dict-en.txt. At load time (engine/spellcheck.js
`buildDictionary`) the two are unioned, junk 2-letter entries are
filtered against a Scrabble whitelist, and a curated US→UK variant
transform adds British spellings (colour, realise, centre...) the
US-spelling SCOWL build lacks.

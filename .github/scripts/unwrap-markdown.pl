#!/usr/bin/perl
# Joins soft-wrapped Markdown lines, so that a paragraph or list item ends up
# on a single line. GitHub renders every newline in a release body as a hard
# line break, which would show CHANGELOG.md's 80-column wrapping verbatim.
#
# Usage: unwrap-markdown.pl < CHANGELOG.md > unwrapped.md
#
# A line is joined onto the previous one unless either of them is blank, or
# the line starts a block of its own: a list item, heading, quote, table row,
# HTML (comments included), a link reference definition, or a code fence.
# Fenced code blocks are passed through untouched, and so are explicit hard
# breaks (a line ending in two spaces or a backslash).
use strict;
use warnings;

my $buf;
my $in_fence = 0;

sub flush {
    print "$buf\n" if defined $buf;
    undef $buf;
}

while (my $line = <STDIN>) {
    chomp $line;

    if ($line =~ /^\s*(```|~~~)/) {
        flush();
        print "$line\n";
        $in_fence = !$in_fence;
        next;
    }
    if ($in_fence || $line =~ /^\s*$/) {
        flush();
        print "$line\n";
        next;
    }
    # Blocks that are always a single line: never joined with anything.
    if ($line =~ /^\s*(#|\||<|\[[^\]]+\]:\s)/) {
        flush();
        print "$line\n";
        next;
    }
    # Blocks that start a new paragraph-like run of lines.
    my $starts_block = $line =~ /^\s*([-*+]\s|\d+[.)]\s|>)/;
    my $hard_break = defined $buf && $buf =~ /(  |\\)$/;
    if (defined $buf && !$starts_block && !$hard_break) {
        $line =~ s/^\s+//;
        $buf .= " $line";
    } else {
        flush();
        $buf = $line;
    }
}
flush();

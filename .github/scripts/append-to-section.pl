#!/usr/bin/perl
# Appends a paragraph to the end of a version's section of a Keep a Changelog
# file, i.e. right before the next `## ` heading or the `<!-- next-url -->`
# marker.
#
# Usage: append-to-section.pl <version> <text> < CHANGELOG.md > out.md
use strict;
use warnings;

my ($version, $text) = @ARGV;
die "usage: $0 <version> <text>\n" unless defined $text;

my @lines = <STDIN>;
my ($start) = grep { $lines[$_] =~ /^## \[\Q$version\E\]/ } 0 .. $#lines;
die "no section for $version\n" unless defined $start;

my $end = $start + 1;
$end++ while $end < @lines && $lines[$end] !~ /^(## |<!-- next-url -->)/;

# Drop the blank lines the section ends with, then put them back after the text.
my $last = $end - 1;
$last-- while $last > $start && $lines[$last] =~ /^\s*$/;

print @lines[0 .. $last], "\n", "$text\n", "\n", @lines[$end .. $#lines];

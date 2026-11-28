#!/usr/bin/env tclsh
# Scoped Valkey/Redis Tcl compatibility driver for expanse-kv (docs/PRD.md §7 Phase 3).
# Sources upstream valkey-io/valkey tests/support/redis.tcl (pinned commit).

if {[llength $argv] < 2} {
    puts stderr "usage: driver.tcl <host> <port>"
    exit 2
}

set host [lindex $argv 0]
set port [lindex $argv 1]
set script_dir [file dirname [file normalize [info script]]]
set valkey_tcl [file join $script_dir "upstream" "tests" "support" "valkey.tcl"]

if {![file exists $valkey_tcl]} {
    puts stderr "error: upstream valkey.tcl not found at $valkey_tcl (run fetch_upstream.sh)"
    exit 2
}

source $valkey_tcl

set ::total_tests 0
set ::passed_tests 0
set ::failed_tests 0
set ::failures [list]

array set ::cat_total  {protocol 0 strings 0 hashes 0 zsets 0 expiry 0}
array set ::cat_passed {protocol 0 strings 0 hashes 0 zsets 0 expiry 0}

proc check {category name body} {
    incr ::total_tests
    incr ::cat_total($category)
    if {[catch {uplevel 1 $body} err]} {
        incr ::failed_tests
        lappend ::failures [list $category $name $err]
        puts "FAIL \[$category\] $name: $err"
    } else {
        incr ::passed_tests
        incr ::cat_passed($category)
    }
}

proc assert_eq {actual expected {msg ""}} {
    if {$actual ne $expected} {
        error "expected '$expected', got '$actual' $msg"
    }
}

proc assert_match {pattern actual {msg ""}} {
    if {![string match $pattern $actual]} {
        error "expected match '$pattern', got '$actual' $msg"
    }
}

proc assert_err {pattern body} {
    if {![catch {uplevel 1 $body} err]} {
        error "expected error matching '$pattern', but succeeded with '$err'"
    }
    if {![string match $pattern $err]} {
        error "expected error matching '$pattern', got '$err'"
    }
}

set r [valkey $host $port]
$r flushdb

# ---------------------------------------------------------------------------
# 1. Protocol
# ---------------------------------------------------------------------------
check protocol "PING returns PONG" {
    assert_eq [$r ping] "PONG"
}
check protocol "PING with argument echoes payload" {
    assert_eq [$r ping "hello-world"] "hello-world"
}
check protocol "ECHO returns binary-safe string" {
    assert_eq [$r echo "a\r\nb\x00c"] "a\r\nb\x00c"
}
check protocol "CLIENT SETNAME and GETNAME roundtrip" {
    assert_eq [$r client setname "tcl-compat"] "OK"
    assert_eq [$r client getname] "tcl-compat"
}
check protocol "CLIENT ID returns positive integer" {
    set cid [$r client id]
    if {![string is integer -strict $cid] || $cid <= 0} {
        error "invalid client id: $cid"
    }
}
check protocol "INFO returns Server and Keyspace sections" {
    set info [$r info]
    assert_match "*# Server*# Keyspace*" $info
}
check protocol "Unknown command returns ERR" {
    assert_err "*ERR unknown command*" {$r nonexistentcmd 1 2}
}
check protocol "Wrong arity returns ERR" {
    assert_err "*ERR wrong number of arguments*" {$r get}
}
check protocol "MULTI/EXEC executes queued commands atomically" {
    assert_eq [$r multi] "OK"
    assert_eq [$r set tx:a 10] "QUEUED"
    assert_eq [$r incr tx:a] "QUEUED"
    assert_eq [$r get tx:a] "QUEUED"
    assert_eq [$r exec] [list OK 11 11]
}
check protocol "MULTI/DISCARD aborts queued commands" {
    assert_eq [$r multi] "OK"
    assert_eq [$r set tx:a 999] "QUEUED"
    assert_eq [$r discard] "OK"
    assert_eq [$r get tx:a] "11"
}
check protocol "Nested MULTI returns ERR" {
    assert_eq [$r multi] "OK"
    assert_err "*MULTI calls can not be nested*" {$r multi}
    assert_eq [$r discard] "OK"
}
check protocol "EXEC without MULTI returns ERR" {
    assert_err "*EXEC without MULTI*" {$r exec}
}
check protocol "HELLO 2 returns server metadata" {
    set res [$r hello 2]
    assert_match "*server expanse-kv*proto 2*" $res
}
check protocol "HELLO 3 switches to RESP3" {
    set r3 [valkey $host $port]
    set res [$r3 hello 3]
    assert_match "*server expanse-kv*proto 3*" $res
    $r3 close
}

# ---------------------------------------------------------------------------
# 2. Strings & Bitmaps
# ---------------------------------------------------------------------------
$r flushdb
check strings "SET and GET basic string" {
    assert_eq [$r set k1 "value1"] "OK"
    assert_eq [$r get k1] "value1"
}
check strings "GET missing key returns empty (nil)" {
    assert_eq [$r get missing_key] ""
}
check strings "SET and GET empty value" {
    assert_eq [$r set empty ""] "OK"
    assert_eq [$r get empty] ""
    assert_eq [$r exists empty] 1
}
check strings "SET NX only sets when absent" {
    assert_eq [$r set nxk v1 nx] "OK"
    assert_eq [$r set nxk v2 nx] ""
    assert_eq [$r get nxk] "v1"
}
check strings "SET XX only sets when present" {
    assert_eq [$r set xxk v1 xx] ""
    assert_eq [$r set xxk v1] "OK"
    assert_eq [$r set xxk v2 xx] "OK"
    assert_eq [$r get xxk] "v2"
}
check strings "SET GET returns previous value" {
    assert_eq [$r set getk old] "OK"
    assert_eq [$r set getk new get] "old"
    assert_eq [$r get getk] "new"
}
check strings "MSET and MGET handle mixed present/absent keys" {
    assert_eq [$r mset m1 a m2 b m3 c] "OK"
    assert_eq [$r mget m1 missing m3] [list a {} c]
}
check strings "INCR, DECR, INCRBY, DECRBY" {
    assert_eq [$r incr cnt] 1
    assert_eq [$r incrby cnt 9] 10
    assert_eq [$r decr cnt] 9
    assert_eq [$r decrby cnt 14] -5
}
check strings "INCR on non-integer string fails" {
    $r set notint "abc"
    assert_err "*not an integer*" {$r incr notint}
}
check strings "DEL and UNLINK return count of removed keys" {
    $r mset d1 1 d2 2 d3 3
    assert_eq [$r del d1 d2 nonexistent] 2
    assert_eq [$r unlink d3] 1
    assert_eq [$r exists d1 d2 d3] 0
}
check strings "TYPE returns string and none" {
    $r set tk v
    assert_eq [$r type tk] "string"
    assert_eq [$r type no_such_key] "none"
}
check strings "DBSIZE and MEMORY USAGE" {
    $r flushdb
    $r mset s1 alpha s2 beta
    assert_eq [$r dbsize] 2
    set mu [$r memory usage s1]
    if {![string is integer -strict $mu] || $mu <= 0} {
        error "expected positive memory usage, got $mu"
    }
}
check strings "SCAN with MATCH prefix* and pagination" {
    $r flushdb
    for {set i 0} {$i < 25} {incr i} {
        $r set [format "org:1:item:%02d" $i] "v$i"
    }
    $r set "other:key" "x"
    set cur "0"
    set seen [list]
    while {1} {
        set page [$r scan $cur match "org:1:item:*" count 7]
        set cur [lindex $page 0]
        foreach k [lindex $page 1] {
            lappend seen $k
        }
        if {$cur eq "0"} break
    }
    assert_eq [llength $seen] 25
    assert_eq [lindex $seen 0] "org:1:item:00"
    assert_eq [lindex $seen end] "org:1:item:24"
}
check strings "KEYS pattern matching" {
    set ks [$r keys "org:1:item:0*"]
    assert_eq [llength $ks] 10
}
check strings "SETBIT, GETBIT, BITCOUNT sparse offsets" {
    $r flushdb
    assert_eq [$r setbit bmp 0 1] 0
    assert_eq [$r setbit bmp 1000000 1] 0
    assert_eq [$r setbit bmp 1000000 1] 1
    assert_eq [$r getbit bmp 1000000] 1
    assert_eq [$r getbit bmp 999999] 0
    assert_eq [$r bitcount bmp] 2
    assert_eq [$r setbit bmp 0 0] 1
    assert_eq [$r bitcount bmp] 1
}

# ---------------------------------------------------------------------------
# 3. Hashes
# ---------------------------------------------------------------------------
$r flushdb
check hashes "HSET, HGET, HLEN, HEXISTS" {
    assert_eq [$r hset h1 f1 v1 f2 v2] 2
    assert_eq [$r hset h1 f1 v1_updated f3 v3] 1
    assert_eq [$r hget h1 f1] "v1_updated"
    assert_eq [$r hget h1 missing] ""
    assert_eq [$r hlen h1] 3
    assert_eq [$r hexists h1 f2] 1
    assert_eq [$r hexists h1 nope] 0
}
check hashes "HMGET returns ordered values and nil for missing" {
    assert_eq [$r hmget h1 f1 nope f3] [list v1_updated {} v3]
}
check hashes "HGETALL returns field-value pairs" {
    set pairs [$r hgetall h1]
    assert_eq [llength $pairs] 6
    assert_eq [dict get $pairs f2] "v2"
}
check hashes "HDEL removes fields and cleans empty hash" {
    assert_eq [$r hdel h1 f1 f2 missing] 2
    assert_eq [$r hlen h1] 1
    assert_eq [$r hdel h1 f3] 1
    assert_eq [$r type h1] "none"
    assert_eq [$r exists h1] 0
}
check hashes "HSCAN paginates all fields" {
    for {set i 0} {$i < 15} {incr i} {
        $r hset hscan_k [format "field:%02d" $i] "val:$i"
    }
    set cur "0"
    set count 0
    while {1} {
        set page [$r hscan hscan_k $cur count 4]
        set cur [lindex $page 0]
        incr count [expr {[llength [lindex $page 1]] / 2}]
        if {$cur eq "0"} break
    }
    assert_eq $count 15
}
check hashes "WRONGTYPE on hash operations against string key" {
    $r set strkey "hello"
    assert_err "*WRONGTYPE*" {$r hget strkey f1}
    assert_err "*WRONGTYPE*" {$r hset strkey f1 v1}
    assert_err "*WRONGTYPE*" {$r hlen strkey}
}

# ---------------------------------------------------------------------------
# 4. Sorted Sets
# ---------------------------------------------------------------------------
$r flushdb
check zsets "ZADD, ZCARD, ZSCORE basic operations" {
    assert_eq [$r zadd zs 10.5 alice 20 bob -3.25 carol] 3
    assert_eq [$r zcard zs] 3
    assert_eq [$r zscore zs alice] "10.5"
    assert_eq [$r zscore zs carol] "-3.25"
    assert_eq [$r zscore zs nobody] ""
}
check zsets "ZADD update existing member returns 0 added" {
    assert_eq [$r zadd zs 99.0 carol] 0
    assert_eq [$r zscore zs carol] "99"
    assert_eq [$r zcard zs] 3
}
check zsets "ZRANGE by rank forward and negative indices" {
    assert_eq [$r zrange zs 0 -1] [list alice bob carol]
    assert_eq [$r zrange zs 1 -1] [list bob carol]
    assert_eq [$r zrange zs -2 -1] [list bob carol]
}
check zsets "ZRANGE REV and ZREVRANGE" {
    assert_eq [$r zrange zs 0 -1 rev] [list carol bob alice]
    assert_eq [$r zrevrange zs 0 1] [list carol bob]
}
check zsets "ZRANGE WITHSCORES" {
    assert_eq [$r zrange zs 0 1 withscores] [list alice 10.5 bob 20]
}
check zsets "ZRANGE BYSCORE inclusive, exclusive, inf, REV, LIMIT" {
    assert_eq [$r zrange zs 10.5 20 byscore] [list alice bob]
    assert_eq [$r zrange zs "(10.5" 20 byscore] [list bob]
    assert_eq [$r zrange zs "-inf" "+inf" byscore limit 1 1] [list bob]
    assert_eq [$r zrange zs 100 10 byscore rev] [list carol bob alice]
}
check zsets "ZRANGEBYSCORE legacy command" {
    assert_eq [$r zrangebyscore zs 10 25] [list alice bob]
}
check zsets "ZSET tie-breaking by member lexicographical order" {
    $r zadd tie 5.0 charlie 5.0 alpha 5.0 bravo
    assert_eq [$r zrange tie 0 -1] [list alpha bravo charlie]
    assert_eq [$r zrange tie 0 -1 rev] [list charlie bravo alpha]
}
check zsets "ZREM and ZREMRANGEBYSCORE" {
    assert_eq [$r zrem zs bob nonexistent] 1
    assert_eq [$r zcard zs] 2
    assert_eq [$r zremrangebyscore zs 0 50] 1
    assert_eq [$r zrange zs 0 -1] [list carol]
}
check zsets "WRONGTYPE on zset operations against string key" {
    $r set zstr "v"
    assert_err "*WRONGTYPE*" {$r zadd zstr 1.0 m}
    assert_err "*WRONGTYPE*" {$r zcard zstr}
    assert_err "*WRONGTYPE*" {$r zrange zstr 0 -1}
}

# ---------------------------------------------------------------------------
# 5. Expiry
# ---------------------------------------------------------------------------
$r flushdb
check expiry "TTL and PTTL on missing and persistent keys" {
    assert_eq [$r ttl missing_ttl] -2
    assert_eq [$r pttl missing_ttl] -2
    $r set persist_k v
    assert_eq [$r ttl persist_k] -1
    assert_eq [$r pttl persist_k] -1
}
check expiry "SET EX and PERSIST" {
    assert_eq [$r set ex_k v ex 60] "OK"
    set t [$r ttl ex_k]
    if {$t <= 0 || $t > 60} {
        error "unexpected ttl: $t"
    }
    assert_eq [$r persist ex_k] 1
    assert_eq [$r ttl ex_k] -1
    assert_eq [$r persist ex_k] 0
}
check expiry "SET KEEPTTL preserves existing expiration" {
    $r set keep_k v1 ex 60
    $r set keep_k v2 keepttl
    assert_eq [$r get keep_k] "v2"
    set t [$r ttl keep_k]
    if {$t <= 0 || $t > 60} {
        error "KEEPTTL lost ttl: $t"
    }
}
check expiry "PEXPIRE expires key cleanly" {
    $r set fast_k "ephemeral"
    assert_eq [$r pexpire fast_k 120] 1
    assert_eq [$r pexpire missing_k 120] 0
    after 200
    assert_eq [$r get fast_k] ""
    assert_eq [$r exists fast_k] 0
    assert_eq [$r ttl fast_k] -2
}
check expiry "EXPIRE on hash key expires entire hash" {
    $r hset exh f1 v1
    assert_eq [$r pexpire exh 120] 1
    after 200
    assert_eq [$r hget exh f1] ""
    assert_eq [$r exists exh] 0
}

$r close

set pct [expr {$::total_tests > 0 ? (100.0 * $::passed_tests / $::total_tests) : 0.0}]
puts ""
puts "=== Scoped Valkey Tcl Compatibility Summary ==="
foreach cat {protocol strings hashes zsets expiry} {
    puts [format "  %-10s %d / %d" $cat $::cat_passed($cat) $::cat_total($cat)]
}
puts [format "TOTAL: %d / %d (%.2f%%)" $::passed_tests $::total_tests $pct]

if {$pct < 98.0} {
    puts stderr "GATE FAILED: pass rate below 98.0%"
    exit 1
}
exit 0

<?php

declare(strict_types=1);

require __DIR__ . '/../src/EggDbException.php';
require __DIR__ . '/../src/Connection.php';
require __DIR__ . '/../src/Statement.php';

$rows = (int) (getenv('DRIVER_BENCH_ROWS') ?: 20000);
$lookups = (int) (getenv('DRIVER_BENCH_LOOKUPS') ?: 5000);

function timed(callable $work): float
{
    $started = hrtime(true);
    $work();
    return (hrtime(true) - $started) / 1e6;
}

function eggdbSide(string $path, int $rows, int $lookups): array
{
    $db = new EggDB\Connection($path);
    $db->execute('CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score REAL)');
    $insert = timed(function () use ($db, $rows): void {
        $db->execute('BEGIN');
        $statement = $db->prepare('INSERT INTO t VALUES (?, ?, ?)');
        for ($at = 1; $at <= $rows; $at++) {
            $statement->execute([$at, "name $at", $at * 0.25]);
        }
        $statement->finalize();
        $db->execute('COMMIT');
    });
    $lookup = timed(function () use ($db, $rows, $lookups): void {
        $statement = $db->prepare('SELECT name FROM t WHERE id = ?');
        for ($at = 1; $at <= $lookups; $at++) {
            foreach ($statement->iterate([$at * 7 % $rows + 1]) as $row) {
            }
        }
        $statement->finalize();
    });
    $scan = timed(function () use ($db): void {
        $total = 0;
        foreach ($db->iterate('SELECT name FROM t') as $row) {
            $total += strlen($row['name']);
        }
    });
    $db->close();
    return [$insert, $lookup, $scan];
}

function sqliteSide(string $path, int $rows, int $lookups): array
{
    $db = new SQLite3($path);
    $db->exec('CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score REAL)');
    $insert = timed(function () use ($db, $rows): void {
        $db->exec('BEGIN');
        $statement = $db->prepare('INSERT INTO t VALUES (?, ?, ?)');
        for ($at = 1; $at <= $rows; $at++) {
            $statement->bindValue(1, $at, SQLITE3_INTEGER);
            $statement->bindValue(2, "name $at", SQLITE3_TEXT);
            $statement->bindValue(3, $at * 0.25, SQLITE3_FLOAT);
            $statement->execute()->finalize();
            $statement->reset();
        }
        $db->exec('COMMIT');
    });
    $lookup = timed(function () use ($db, $rows, $lookups): void {
        $statement = $db->prepare('SELECT name FROM t WHERE id = ?');
        for ($at = 1; $at <= $lookups; $at++) {
            $statement->bindValue(1, $at * 7 % $rows + 1, SQLITE3_INTEGER);
            $result = $statement->execute();
            while ($result->fetchArray(SQLITE3_ASSOC)) {
            }
            $result->finalize();
            $statement->reset();
        }
    });
    $scan = timed(function () use ($db): void {
        $total = 0;
        $result = $db->query('SELECT name FROM t');
        while ($row = $result->fetchArray(SQLITE3_ASSOC)) {
            $total += strlen($row['name']);
        }
    });
    $db->close();
    return [$insert, $lookup, $scan];
}

$directory = sys_get_temp_dir() . '/eggdb-php-bench-' . getmypid();
@mkdir($directory);
$egg = eggdbSide("$directory/bench.eggdb", $rows, $lookups);
$lite = sqliteSide("$directory/bench.sqlite", $rows, $lookups);
foreach (['insert', 'lookup', 'scan'] as $at => $workload) {
    printf("php %s %.2f %.2f\n", $workload, $egg[$at], $lite[$at]);
}

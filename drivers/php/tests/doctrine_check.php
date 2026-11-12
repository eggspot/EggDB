<?php

declare(strict_types=1);

$autoload = getenv('EGGDB_DOCTRINE_AUTOLOAD') ?: __DIR__ . '/../vendor/autoload.php';
if (!is_file($autoload)) {
    echo "doctrine check: skipped, Doctrine DBAL is not installed\n";
    exit(0);
}
require $autoload;
foreach (['EggDbException', 'Connection', 'Doctrine/Exception', 'Doctrine/Result', 'Doctrine/Statement', 'Doctrine/Connection', 'Doctrine/Driver'] as $file) {
    require_once __DIR__ . "/../src/$file.php";
}

function check(bool $condition, string $what): void
{
    if (!$condition) {
        fwrite(STDERR, "doctrine check failed: $what\n");
        exit(1);
    }
}

$connection = Doctrine\DBAL\DriverManager::getConnection(['driverClass' => EggDB\Doctrine\Driver::class, 'memory' => true]);
$connection->executeStatement('CREATE TABLE eggs (id INTEGER PRIMARY KEY, colour TEXT NOT NULL, weight INTEGER)');
$connection->insert('eggs', ['colour' => 'brown', 'weight' => 61]);
check((int) $connection->lastInsertId() === 1, 'the first id is 1');
$connection->insert('eggs', ['colour' => 'white', 'weight' => 58]);
check($connection->update('eggs', ['weight' => 59], ['colour' => 'white']) === 1, 'one row updated');
$rows = $connection->fetchAllAssociative('SELECT colour, weight FROM eggs WHERE weight > :least ORDER BY id', ['least' => 50]);
check($rows === [['colour' => 'brown', 'weight' => 61], ['colour' => 'white', 'weight' => 59]], 'rows by name');
$connection->transactional(fn ($inner) => $inner->delete('eggs', ['colour' => 'brown']));
check((int) $connection->fetchOne('SELECT count(*) FROM eggs') === 1, 'one egg left');
$builder = $connection->createQueryBuilder()->select('colour')->from('eggs');
check($builder->executeQuery()->fetchFirstColumn() === ['white'], 'the query builder runs');
echo "doctrine check: passed\n";

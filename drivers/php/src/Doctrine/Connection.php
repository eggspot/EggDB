<?php

declare(strict_types=1);

namespace EggDB\Doctrine;

use Doctrine\DBAL\Driver\Connection as DriverConnection;
use EggDB\EggDbException;

final class Connection implements DriverConnection
{
    public function __construct(private \EggDB\Connection $connection)
    {
    }

    public function run(string $sql, array $parameters = []): array
    {
        try {
            return $this->connection->execute($sql, $parameters);
        } catch (EggDbException $error) {
            throw Exception::from($error);
        }
    }

    public function prepare(string $sql): Statement
    {
        return new Statement($this, $sql);
    }

    public function query(string $sql): Result
    {
        return Result::of($this->run($sql));
    }

    public function quote(string $value): string
    {
        return "'" . str_replace("'", "''", $value) . "'";
    }

    public function exec(string $sql): int
    {
        return Result::of($this->run($sql))->rowCount();
    }

    public function lastInsertId(): int|string
    {
        return Result::of($this->run('SELECT last_insert_rowid()'))->fetchOne();
    }

    public function beginTransaction(): void
    {
        $this->run('BEGIN');
    }

    public function commit(): void
    {
        $this->run('COMMIT');
    }

    public function rollBack(): void
    {
        $this->run('ROLLBACK');
    }

    public function getServerVersion(): string
    {
        return (string) Result::of($this->run('SELECT sqlite_version()'))->fetchOne();
    }

    public function getNativeConnection(): \EggDB\Connection
    {
        return $this->connection;
    }
}

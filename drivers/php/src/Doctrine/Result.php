<?php

declare(strict_types=1);

namespace EggDB\Doctrine;

use Doctrine\DBAL\Driver\Result as DriverResult;

final class Result implements DriverResult
{
    private int $at = 0;

    private function __construct(private array $columns, private array $rows, private int $changed)
    {
    }

    public static function of(array $results): self
    {
        for ($index = count($results) - 1; $index >= 0; $index--) {
            if ($results[$index]['columns'] !== []) {
                $columns = $results[$index]['columns'];
                $rows = array_map(
                    fn (array $row) => array_map(fn (string $column) => $row[$column] ?? null, $columns),
                    $results[$index]['rows'],
                );
                return new self($columns, $rows, count($rows));
            }
        }
        $changed = 0;
        foreach ($results as $result) {
            $words = explode(' ', $result['tag']);
            if (in_array($words[0], ['INSERT', 'UPDATE', 'DELETE'], true)) {
                $changed += (int) end($words);
            }
        }
        return new self([], [], $changed);
    }

    public function fetchNumeric(): array|false
    {
        return $this->rows[$this->at++] ?? false;
    }

    public function fetchAssociative(): array|false
    {
        $row = $this->fetchNumeric();
        return $row === false ? false : array_combine($this->columns, $row);
    }

    public function fetchOne(): mixed
    {
        $row = $this->fetchNumeric();
        return $row === false ? false : $row[0];
    }

    public function fetchAllNumeric(): array
    {
        $rest = array_slice($this->rows, $this->at);
        $this->at = count($this->rows);
        return $rest;
    }

    public function fetchAllAssociative(): array
    {
        return array_map(fn (array $row) => array_combine($this->columns, $row), $this->fetchAllNumeric());
    }

    public function fetchFirstColumn(): array
    {
        return array_map(fn (array $row) => $row[0], $this->fetchAllNumeric());
    }

    public function rowCount(): int
    {
        return $this->changed;
    }

    public function columnCount(): int
    {
        return count($this->columns);
    }

    public function getColumnName(int $index): string
    {
        return $this->columns[$index] ?? throw new \OutOfBoundsException("no column $index");
    }

    public function free(): void
    {
        $this->rows = [];
        $this->at = 0;
    }
}

<?php

declare(strict_types=1);

namespace EggDB\Doctrine;

use Doctrine\DBAL\Driver\Statement as DriverStatement;
use Doctrine\DBAL\ParameterType;

final class Statement implements DriverStatement
{
    private string $positional;
    private array $order = [];
    private array $values = [];

    public function __construct(private Connection $connection, string $sql)
    {
        [$this->positional, $this->order] = self::positional($sql);
    }

    private static function positional(string $sql): array
    {
        $out = '';
        $order = [];
        $quote = null;
        $next = 1;
        $length = strlen($sql);
        for ($at = 0; $at < $length; $at++) {
            $character = $sql[$at];
            if ($quote !== null) {
                if ($character === $quote) {
                    $quote = null;
                }
                $out .= $character;
                continue;
            }
            if ($character === "'" || $character === '"') {
                $quote = $character;
                $out .= $character;
                continue;
            }
            if ($character === '?') {
                $order[] = $next++;
                $out .= '?';
                continue;
            }
            if ($character === ':' && $at + 1 < $length && (ctype_alpha($sql[$at + 1]) || $sql[$at + 1] === '_')) {
                $end = $at + 1;
                while ($end < $length && (ctype_alnum($sql[$end]) || $sql[$end] === '_')) {
                    $end++;
                }
                $order[] = substr($sql, $at + 1, $end - $at - 1);
                $out .= '?';
                $at = $end - 1;
                continue;
            }
            $out .= $character;
        }
        return [$out, $order];
    }

    public function bindValue(int|string $param, mixed $value, ParameterType $type = ParameterType::STRING): void
    {
        $key = is_string($param) ? ltrim($param, ':') : $param;
        $this->values[$key] = match (true) {
            $value === null || $type === ParameterType::NULL => null,
            $type === ParameterType::BOOLEAN => $value ? 1 : 0,
            $type === ParameterType::INTEGER => (int) $value,
            is_resource($value) => stream_get_contents($value),
            default => $value,
        };
    }

    public function execute(): Result
    {
        $parameters = array_map(fn ($key) => $this->values[$key] ?? null, $this->order);
        return Result::of($this->connection->run($this->positional, $parameters));
    }
}

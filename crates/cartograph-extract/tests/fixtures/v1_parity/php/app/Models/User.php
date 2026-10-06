<?php
namespace App\Models;

use App\Contracts\Repo;
use JsonSerializable;

const MODEL_VERSION = '1.0';

interface Repo
{
    public function find(int $id): ?User;
}

abstract class Model
{
    public static function where(string $col, $value): static
    {
        return new static();
    }

    public function get(): array
    {
        return [];
    }

    protected static function boot(): void
    {
    }
}

trait Loggable
{
    public function log(string $msg): void
    {
        echo $msg;
    }
}

trait Timestamps
{
}

enum Suit: string
{
    case Hearts = 'H';
    case Spades = 'S';

    public function label(): string
    {
        return ucfirst($this->name);
    }
}

class User extends Model implements JsonSerializable, \Countable
{
    use Loggable, Timestamps;

    const ROLE_ADMIN = 'admin';
    public const ROLE_USER = 'user';

    private Repo $repo;
    protected ?string $email = null;
    public static int $count = 0;

    public function __construct(Repo $repo)
    {
        $this->repo = $repo;
    }

    public static function find(int $id): ?self
    {
        return new self(new UserRepo());
    }

    public static function make(): self
    {
        return self::find(1);
    }

    public function save(): bool
    {
        $this->log('saving');
        $this->touch();
        return true;
    }

    private function touch(): void
    {
        parent::boot();
    }

    public function jsonSerialize(): mixed
    {
        return [];
    }

    public function count(): int
    {
        return 0;
    }
}

class UserRepo implements Repo
{
    public function find(int $id): ?User
    {
        return User::find($id);
    }
}

class ApiClient
{
    public static function for(string $credential): ?self
    {
        return new self();
    }

    public function createOrder(): void
    {
    }
}

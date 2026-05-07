<?php

namespace App\Repositories;

use App\Contracts\UserRepositoryInterface;
use App\Models\User;
use App\Enums\UserStatus;
use Illuminate\Support\Collection;

/**
 * Doctrine-style repository for User entities using PHP 8.1+ features.
 */
class UserRepository implements UserRepositoryInterface
{
    public function __construct(
        private readonly \PDO $pdo,
    ) {}

    /**
     * Find a user by primary key, returning null when not found.
     */
    public function find(int $id): ?User
    {
        $stmt = $this->pdo->prepare('SELECT * FROM users WHERE id = :id LIMIT 1');
        $stmt->execute(compact('id'));
        $row = $stmt->fetch(\PDO::FETCH_ASSOC);
        return $row ? $this->hydrate($row) : null;
    }

    /**
     * Return all active users, sorted by name.
     */
    public function findAllActive(): Collection
    {
        $status = UserStatus::Active->value;
        $stmt = $this->pdo->prepare('SELECT * FROM users WHERE status = :status ORDER BY name');
        $stmt->execute(compact('status'));
        $rows = $stmt->fetchAll(\PDO::FETCH_ASSOC);
        return collect(array_map($this->hydrate(...), $rows));
    }

    /**
     * Persist a new user and return it with a generated id.
     */
    public function save(User $user): User
    {
        $stmt = $this->pdo->prepare(
            'INSERT INTO users (name, email, status) VALUES (:name, :email, :status)'
        );
        $stmt->execute([
            'name'   => $user->name,
            'email'  => $user->email,
            'status' => $user->status->value,
        ]);
        return $user->withId((int) $this->pdo->lastInsertId());
    }

    /**
     * Delete a user by id. Returns true when a row was removed.
     */
    public function delete(int $id): bool
    {
        $stmt = $this->pdo->prepare('DELETE FROM users WHERE id = :id');
        $stmt->execute(compact('id'));
        return $stmt->rowCount() > 0;
    }

    // ── Private helpers ──────────────────────────────────────────────────────

    /**
     * Map a raw database row to a User value-object.
     */
    private function hydrate(array $row): User
    {
        return new User(
            id:     (int) $row['id'],
            name:   $row['name'],
            email:  $row['email'],
            status: UserStatus::from($row['status']),
        );
    }
}

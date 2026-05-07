<?php

namespace App\EventListeners;

use App\Contracts\EventListenerInterface;
use App\Events\UserRegisteredEvent;
use App\Services\MailService;
use App\Services\AuditService;
use Psr\Log\LoggerInterface;

/**
 * Handles the UserRegistered domain event.
 * Sends welcome email and records an audit trail.
 */
class UserRegisteredListener implements EventListenerInterface
{
    use LogsActivity;
    use SendsNotifications;

    public function __construct(
        private readonly MailService    $mailer,
        private readonly AuditService   $audit,
        private readonly LoggerInterface $logger,
    ) {}

    /**
     * Handle the event.
     */
    public function handle(UserRegisteredEvent $event): void
    {
        $user = $event->user;

        $this->mailer->sendWelcome($user);
        $this->audit->record('user_registered', ['id' => $user->id]);
        $this->logger->info('UserRegistered handled', ['user_id' => $user->id]);
        $this->logActivity($user, 'registered');
    }

    /**
     * Determine whether the listener should be queued.
     */
    public function shouldQueue(): bool
    {
        return true;
    }

    /**
     * Get the name of the listener's queue.
     */
    public function viaQueue(): string
    {
        return 'notifications';
    }
}

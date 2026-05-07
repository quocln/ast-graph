<?php

namespace App\Http\Controllers;

use App\Models\User;
use App\Services\UserService;
use Illuminate\Http\Request;
use Illuminate\Http\JsonResponse;

/**
 * Handles HTTP requests for user resources.
 */
class UserController extends Controller
{
    public function __construct(
        private readonly UserService $userService
    ) {}

    /**
     * Display a listing of users.
     */
    public function index(Request $request): JsonResponse
    {
        $users = $this->userService->paginate($request->integer('per_page', 15));
        return response()->json($users);
    }

    /**
     * Display the specified user.
     */
    public function show(int $id): JsonResponse
    {
        $user = User::findOrFail($id);
        return response()->json($user);
    }

    /**
     * Store a new user.
     */
    public function store(Request $request): JsonResponse
    {
        $validated = $request->validate([
            'name'  => 'required|string|max:255',
            'email' => 'required|email|unique:users',
        ]);
        $user = $this->userService->create($validated);
        return response()->json($user, 201);
    }

    /**
     * Update the specified user.
     */
    public function update(Request $request, int $id): JsonResponse
    {
        $user = User::findOrFail($id);
        $this->userService->update($user, $request->all());
        return response()->json($user);
    }

    /**
     * Remove the specified user.
     */
    public function destroy(int $id): JsonResponse
    {
        User::findOrFail($id)->delete();
        return response()->json(null, 204);
    }
}

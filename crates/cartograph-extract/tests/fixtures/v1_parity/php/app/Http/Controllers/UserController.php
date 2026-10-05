<?php
namespace App\Http\Controllers;

use App\Models\User;
use App\Models\UserRepo as Repository;
use App\Models\{Suit, ApiClient};
use function App\Support\helper_fn;
use App\Support\Config;
use Illuminate\Support\Facades\Cache;

include_once 'app/Support/helpers.php';
require_once("app/Models/User.php");
include 'partials/header.php';
require __DIR__ . '/dynamic.php';

class BaseController
{
    protected function authorize(string $ability): bool
    {
        return true;
    }
}

class UserController extends BaseController
{
    private Repository $users;

    public function __construct()
    {
        $this->users = new Repository();
    }

    public function index(): array
    {
        $this->authorize('view');
        $user = User::find(1);
        $user->save();
        $all = User::where('active', 1)->get();
        $cached = Cache::get('users');
        $label = Suit::Hearts->label();
        $made = User::make();
        ApiClient::for('client-a')->createOrder();
        helper_fn(new Config());
        \App\Support\format_name('x');
        return [$user, $all, $cached, $label, $made];
    }

    public function show(int $id): ?User
    {
        return $this->users->find($id);
    }

    public function store(): void
    {
        $repo = new Repository();
        $repo->find(2);
    }
}

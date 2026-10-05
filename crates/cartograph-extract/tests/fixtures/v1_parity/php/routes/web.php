<?php
use Illuminate\Support\Facades\Route;
use App\Http\Controllers\UserController;

Route::get('/users', [UserController::class, 'index']);
Route::post('/users', [UserController::class, 'store']);
Route::any('/ping', 'UserController@index');
Route::delete('/users/{id}', [UserController::class, 'destroy']);
Route::resource('photos', 'PhotoController');
Route::apiResource('posts', 'PostController');

<?php
namespace App\Controller;

use Symfony\Component\Routing\Attribute\Route;

#[Route('/blog', name: 'blog_')]
class BlogController
{
    #[Route('/list', name: 'list', methods: ['GET'])]
    public function list(): string
    {
        return $this->render('list');
    }

    #[Route('/post/{slug}', methods: ['GET', 'POST'])]
    public function show(string $slug): string
    {
        return $this->render($slug);
    }

    private function render(string $view): string
    {
        return $view;
    }
}

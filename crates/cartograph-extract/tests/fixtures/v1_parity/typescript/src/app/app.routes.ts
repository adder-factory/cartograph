import { Routes, provideRouter } from '@angular/router';
import { Component } from '@angular/core';

@Component({
  selector: 'app-home',
  template: '<p>home</p>',
})
export class HomeComponent {}

@Component({ selector: 'app-dash', template: '<p>dash</p>' })
export class DashComponent {}

export const routes: Routes = [
  { path: '', component: HomeComponent },
  { path: 'admin', loadChildren: () => import('./admin.routes') },
  {
    path: 'settings',
    loadComponent: () => import('./settings.component').then((m) => m.SettingsComponent),
  },
];

export const appConfig = {
  providers: [provideRouter([{ path: 'dash', component: DashComponent }])],
};

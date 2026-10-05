program ShopApp;

uses
  Vcl.Forms,
  MainUnit in 'MainUnit.pas' {MainForm},
  UAuth in 'UAuth.pas';

{$R *.res}

begin
  Application.Initialize;
  Application.CreateForm(TMainForm, MainForm);
  Application.Run;
end.

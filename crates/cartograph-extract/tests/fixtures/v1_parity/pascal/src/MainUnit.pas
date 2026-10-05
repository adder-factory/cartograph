unit MainUnit;

interface

uses
  System.SysUtils, Vcl.Forms, Vcl.StdCtrls, UAuth;

type
  TMainForm = class(TForm)
    Button1: TButton;
    procedure FormCreate(Sender: TObject);
    procedure Button1Click(Sender: TObject);
  private
    FCount: Integer;
  public
    property Count: Integer read FCount;
  end;

var
  MainForm: TMainForm;

implementation

{$R *.dfm}

procedure TMainForm.FormCreate(Sender: TObject);
begin
  FCount := 0;
  ShowMessage('hi');
end;

procedure TMainForm.Button1Click(Sender: TObject);
var
  Auth: TAuthService;
begin
  Auth := TAuthService.Instance;
  Auth.Login('a', 'b');
  Inc(FCount);
end;

end.

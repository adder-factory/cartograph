unit UAuth;

interface

uses
  System.SysUtils,
  System.Classes,
  UTypes;

type
  ITokenValidator = interface
    ['{22222222-2222-2222-2222-222222222222}']
    function Validate(const AToken: string): Boolean;
  end;

  TPerson = class(TObject)
  private
    FName: string;
  public
    property Name: string read FName write FName;
    procedure Greet;
  end;

  TAuthService = class(TInterfacedObject, ITokenValidator, ILogger)
  private
    FToken: string;
    FLoginCount: Integer;
    procedure IncLoginCount;
  protected
    function GetToken: string;
  public
    constructor Create;
    destructor Destroy; override;
    function Validate(const AToken: string): Boolean;
    function Login(const AUser, APass: string): string;
    procedure Log(const AMsg: string);
    class function Instance: TAuthService; static;
    property Token: string read GetToken;
  end;

var
  GlobalAuth: TAuthService;

procedure DoHelper(N: Integer);

implementation

procedure DoHelper(N: Integer);
begin
  WriteLn(IntToStr(N));
end;

procedure TPerson.Greet;
begin
  WriteLn(FName);
  DoHelper(1);
end;

constructor TAuthService.Create;
begin
  inherited Create;
  FLoginCount := 0;
end;

destructor TAuthService.Destroy;
begin
  inherited;
end;

procedure TAuthService.IncLoginCount;
begin
  Inc(FLoginCount);
end;

function TAuthService.GetToken: string;
begin
  Result := FToken;
end;

function TAuthService.Validate(const AToken: string): Boolean;
begin
  Result := AToken = FToken;
end;

function TAuthService.Login(const AUser, APass: string): string;
var
  P: TPoint;
begin
  IncLoginCount;
  P.X := 0;
  Log('login ' + AUser);
  if Distance(P, P) < MaxItems then
    FToken := AUser + APass;
  Result := GetToken;
end;

procedure TAuthService.Log(const AMsg: string);
begin
  WriteLn(AppName + ': ' + AMsg);
end;

class function TAuthService.Instance: TAuthService;
begin
  if GlobalAuth = nil then
    GlobalAuth := TAuthService.Create;
  Result := GlobalAuth;
end;

end.

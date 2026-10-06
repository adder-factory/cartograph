unit UTypes;

interface

uses
  System.SysUtils;

const
  MaxItems = 10;
  AppName = 'Shop';

type
  TColor = (clRed, clGreen, clBlue);
  TUserName = string;
  TIds = array of Integer;

  TPoint = record
    X: Double;
    Y: Double;
  end;

  ILogger = interface
    ['{11111111-1111-1111-1111-111111111111}']
    procedure Log(const AMsg: string);
  end;

  IGreeter = interface(ILogger)
    procedure Greet;
  end;

function Distance(const A, B: TPoint): Double;

implementation

function Distance(const A, B: TPoint): Double;
begin
  Result := Sqrt(Sqr(A.X - B.X) + Sqr(A.Y - B.Y));
end;

end.
